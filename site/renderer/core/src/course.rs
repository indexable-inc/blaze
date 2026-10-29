//! The course as a graph (content/schema.flecs: Lesson, Update, Theme, Requires, InTheme), laid out for the home
//! page's path. Built once from the world; render.rs draws it, side.rs reads it.
//!
//! - The nodes are the Lesson and Update pages under a Course's `(Lessons, page)` targets.
//! - An edge `(Requires, page)` says which node to learn first. A node may require one in its own theme or an earlier
//!   one, never a later one, and the edges must not make a cycle: either fails the build, naming the pages.
//! - Each theme is a band of the path under its own heading, top to bottom by `Theme.order`. Within a band the nodes
//!   are in topological order, the next one always the smallest ready key: a lesson by its `order`; an update right
//!   after the latest node it requires (then by date, version and path), so it sits next to the lesson it extends.
//!
//! The layout is a grid: one node per row, rows top to bottom, and columns to the right of a gutter. An edge within a
//! band runs down its target's column, bending into it just below its source, so that column must be free of nodes
//! and of other edges in the rows between: each node takes the first column where that holds (a lesson from column 0,
//! an update that requires a node of its own band from column 1, so it branches off). An edge from an earlier band
//! crosses the divider on a track of the gutter, left of column 0: one track per source node, the fewest that do not
//! overlap, the earliest source innermost. client/learn.ts measures the pins and strokes these routes; it decides
//! nothing about where they go.

use std::collections::HashSet;

use crate::js;
use crate::world::{Entity, Id, World};

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub id: Id,
    /// The entity's name under `themes` (`basics`).
    pub name: String,
    pub title: String,
    pub line: String,
    /// A class suffix (`theme-canary`), empty for the ink.
    pub accent: String,
    /// What the changelog calls an update in it (`Canary`), empty for nothing.
    pub tag: String,
    pub dashed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// A lesson, and its number on the course (1 first, by `Lesson.order`).
    Lesson(usize),
    /// An update: its date (YYYY-MM-DD) and version (0.YYYYMMDD.N).
    Update { date: String, version: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: Id,
    pub kind: Kind,
    /// Its key in the reader's progress (client/lib/progress.ts): a lesson's name (`builds`), an update's
    /// `<parent>/<name>` (`updates/why-rebuilt`).
    pub key: String,
    /// Index into `Course::themes`.
    pub theme: usize,
    /// The nodes it requires, as indexes into `Course::nodes`.
    pub requires: Vec<usize>,
    /// Its row is its index in `Course::nodes`; this is its column.
    pub col: usize,
}

impl Node {
    pub fn is_update(&self) -> bool {
        matches!(self.kind, Kind::Update { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    /// For an edge from an earlier band: its track in the gutter (0 nearest column 0). None within a band.
    pub track: Option<usize>,
}

/// The most columns and gutter tracks styles/learn.css has rules for (`data-col`, `data-cols`, `data-tracks`: the
/// site's CSP refuses inline styles, so the layout reaches the CSS as attributes, not custom properties).
pub const MAX_COLS: usize = 4;
pub const MAX_TRACKS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct Course {
    /// The bands, top to bottom; only those with a node.
    pub themes: Vec<Theme>,
    /// In row order: band by band, each band's nodes in topological order.
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Columns used (at least 1).
    pub cols: usize,
    /// Gutter tracks used.
    pub tracks: usize,
}

fn field(e: &Entity, comp: &str, k: &str) -> String {
    js::string(e.value(comp).and_then(|v| v.get(k)))
}

/// `0.20261028.3` as numbers, so 0.20261028.10 sorts after .9.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The order a ready node is taken in: its anchor (a lesson's order; for an update, the latest anchor it requires),
/// lessons before updates at the same anchor, then date, version and path.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
struct Key(f64, u8, String, Vec<u64>, String);

impl Course {
    /// The lessons, by number.
    pub fn lessons(&self) -> impl Iterator<Item = &Node> {
        let mut v: Vec<&Node> = self.nodes.iter().filter(|n| !n.is_update()).collect();
        v.sort_by_key(|n| match n.kind {
            Kind::Lesson(i) => i,
            Kind::Update { .. } => 0,
        });
        v.into_iter()
    }

    /// The updates, newest first (by date, then version, then path).
    pub fn updates_newest_first<'a>(&'a self, world: &World) -> Vec<&'a Node> {
        let mut v: Vec<&Node> = self.nodes.iter().filter(|n| n.is_update()).collect();
        v.sort_by(|a, b| {
            let k = |n: &Node| match &n.kind {
                Kind::Update { date, version } => (date.clone(), version_key(version)),
                Kind::Lesson(_) => (String::new(), Vec::new()),
            };
            k(b).cmp(&k(a)).then_with(|| world.get(a.id).path.cmp(&world.get(b.id).path))
        });
        v
    }

    /// "lesson 2", "lessons 3 and 4": the lessons a node requires, for an update's eyebrow. Empty when it requires
    /// none.
    pub fn extends(&self, node: usize) -> String {
        let mut ns: Vec<usize> = self.nodes[node]
            .requires
            .iter()
            .filter_map(|&r| match self.nodes[r].kind {
                Kind::Lesson(i) => Some(i),
                Kind::Update { .. } => None,
            })
            .collect();
        ns.sort();
        match ns.as_slice() {
            [] => String::new(),
            [a] => format!("lesson {a}"),
            [rest @ .., last] => format!(
                "lessons {} and {last}",
                rest.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// The course of a `Course` widget whose lessons and updates are under `sources`.
pub fn build(world: &World, sources: &[Id]) -> Result<Course, String> {
    // The nodes, with their theme and what they require, before any order.
    struct Raw {
        id: Id,
        kind: Kind,
        key: String,
        theme: Id,
        requires: Vec<Id>,
    }
    let mut raw: Vec<Raw> = Vec::new();
    let mut lessons: Vec<Id> = Vec::new();
    for &s in sources {
        lessons.extend(world.lessons(s));
    }
    {
        let order =
            |id: Id| js::to_number(world.get(id).value("Lesson").and_then(|v| v.get("order")));
        lessons.sort_by(|&a, &b| {
            order(a).total_cmp(&order(b)).then_with(|| world.get(a).path.cmp(&world.get(b).path))
        });
    }
    for &s in sources {
        for &c in &world.get(s).children {
            let e = world.get(c);
            if !e.is_a("Page") || !(e.has("Lesson") || e.has("Update")) {
                continue;
            }
            if e.has("Lesson") && e.has("Update") {
                return Err(format!("{}: a page is a Lesson or an Update, not both", e.path));
            }
            let kind = if e.has("Lesson") {
                Kind::Lesson(
                    lessons.iter().position(|&l| l == c).expect("a lesson under a source") + 1,
                )
            } else {
                let (date, version) = (field(e, "Update", "date"), field(e, "Update", "version"));
                let ok_date = date.len() == 10
                    && date.bytes().enumerate().all(|(i, b)| {
                        if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() }
                    });
                if !ok_date {
                    return Err(format!("{}: Update date {date:?} is not YYYY-MM-DD", e.path));
                }
                let parts: Vec<&str> = version.split('.').collect();
                if parts.len() != 3
                    || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
                {
                    return Err(format!(
                        "{}: Update version {version:?} is not 0.YYYYMMDD.N",
                        e.path
                    ));
                }
                Kind::Update { date, version }
            };
            let key = match kind {
                Kind::Lesson(_) => e.name.clone(),
                Kind::Update { .. } => format!("{}/{}", world.get(s).name, e.name),
            };
            let theme_path = e.targets("InTheme").and_then(|t| t.first()).ok_or_else(|| {
                format!("{}: a lesson or update needs (InTheme, themes.<name>)", e.path)
            })?;
            let theme = world
                .lookup(theme_path)
                .filter(|&t| world.get(t).has("Theme"))
                .ok_or_else(|| format!("{}: (InTheme, {theme_path}) is not a Theme", e.path))?;
            let mut requires = Vec::new();
            for r in e.targets("Requires").unwrap_or_default() {
                let id = world
                    .lookup(r)
                    .ok_or_else(|| format!("{}: (Requires, {r}) names nothing", e.path))?;
                requires.push(id);
            }
            raw.push(Raw { id: c, kind, key, theme, requires });
        }
    }
    if raw.is_empty() {
        return Err("a Course with no lessons".into());
    }
    // The themes, top to bottom.
    let mut theme_ids: Vec<Id> = Vec::new();
    for r in &raw {
        if !theme_ids.contains(&r.theme) {
            theme_ids.push(r.theme);
        }
    }
    let theme_order =
        |t: Id| js::to_number(world.get(t).value("Theme").and_then(|v| v.get("order")));
    theme_ids.sort_by(|&a, &b| {
        theme_order(a)
            .total_cmp(&theme_order(b))
            .then_with(|| world.get(a).path.cmp(&world.get(b).path))
    });
    let themes: Vec<Theme> = theme_ids
        .iter()
        .map(|&t| {
            let e = world.get(t);
            Theme {
                id: t,
                name: e.name.clone(),
                title: field(e, "Theme", "title"),
                line: field(e, "Theme", "line"),
                accent: field(e, "Theme", "accent"),
                tag: field(e, "Theme", "tag"),
                dashed: e.has_tag("Dashed"),
            }
        })
        .collect();
    let band =
        |t: Id| theme_ids.iter().position(|&x| x == t).expect("every node's theme is listed");
    // Every edge is to a node of the course, in the same band or an earlier one.
    let at = |id: Id| raw.iter().position(|r| r.id == id);
    for r in &raw {
        for &q in &r.requires {
            let i = at(q).ok_or_else(|| {
                format!(
                    "{}: (Requires, {}) is not a lesson or update on this course",
                    world.get(r.id).path,
                    world.get(q).path
                )
            })?;
            if band(raw[i].theme) > band(r.theme) {
                return Err(format!(
                    "{}: requires {}, which is in a later theme ({})",
                    world.get(r.id).path,
                    world.get(q).path,
                    world.get(raw[i].theme).path
                ));
            }
        }
    }

    // Rows: band by band, Kahn's order by Key.
    let mut order: Vec<usize> = Vec::new(); // indexes into raw, in row order
    let mut anchor: Vec<Option<f64>> = vec![None; raw.len()];
    for b in 0..themes.len() {
        let mut left: Vec<usize> = (0..raw.len()).filter(|&i| band(raw[i].theme) == b).collect();
        while !left.is_empty() {
            let placed = |i: usize| order.contains(&i);
            let mut best: Option<(Key, usize)> = None;
            for &i in &left {
                let reqs: Vec<usize> =
                    raw[i].requires.iter().map(|&q| at(q).expect("checked")).collect();
                if !reqs.iter().all(|&q| placed(q)) {
                    continue;
                }
                let e = world.get(raw[i].id);
                let key = match &raw[i].kind {
                    Kind::Lesson(_) => {
                        let o = js::to_number(e.value("Lesson").and_then(|v| v.get("order")));
                        Key(o, 0, String::new(), Vec::new(), e.path.clone())
                    }
                    Kind::Update { date, version } => {
                        let a = reqs
                            .iter()
                            .filter_map(|&q| anchor[q])
                            .fold(f64::NEG_INFINITY, f64::max);
                        let a = if a.is_finite() { a } else { 0.0 };
                        Key(a, 1, date.clone(), version_key(version), e.path.clone())
                    }
                };
                if best.as_ref().is_none_or(|(k, _)| key < *k) {
                    best = Some((key, i));
                }
            }
            let Some((key, i)) = best else {
                let names: Vec<&str> =
                    left.iter().map(|&i| world.get(raw[i].id).path.as_str()).collect();
                return Err(format!(
                    "the course's Requires make a cycle among {}",
                    names.join(", ")
                ));
            };
            anchor[i] = Some(key.0);
            order.push(i);
            left.retain(|&x| x != i);
        }
    }
    let row_of = |i: usize| order.iter().position(|&x| x == i).expect("every node has a row");

    // Columns, greedily, row by row.
    let mut used: HashSet<(usize, usize)> = HashSet::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    for (row, &i) in order.iter().enumerate() {
        let r = &raw[i];
        let reqs: Vec<usize> =
            r.requires.iter().map(|&q| row_of(at(q).expect("checked"))).collect();
        let same: Vec<usize> =
            reqs.iter().copied().filter(|&q| nodes[q].theme == band(r.theme)).collect();
        let min = match r.kind {
            Kind::Update { .. } if !same.is_empty() => 1,
            _ => 0,
        };
        let free = |c: usize| same.iter().all(|&q| (q + 1..row).all(|rr| !used.contains(&(rr, c))));
        let col = (min..).find(|&c| free(c)).expect("a column to the right is always free");
        used.insert((row, col));
        for &q in &same {
            for rr in q + 1..row {
                used.insert((rr, col));
            }
        }
        for &q in &reqs {
            edges.push(Edge { from: q, to: row, track: None });
        }
        nodes.push(Node {
            id: r.id,
            kind: r.kind.clone(),
            key: r.key.clone(),
            theme: band(r.theme),
            requires: reqs,
            col,
        });
    }

    // Tracks for the edges that cross a divider: one per source, intervals coloured greedily by start.
    let mut spans: Vec<(usize, usize)> = Vec::new(); // (source row, last target row)
    for e in &edges {
        if nodes[e.from].theme == nodes[e.to].theme {
            continue;
        }
        match spans.iter_mut().find(|(s, _)| *s == e.from) {
            Some((_, end)) => *end = (*end).max(e.to),
            None => spans.push((e.from, e.to)),
        }
    }
    spans.sort();
    let mut ends: Vec<usize> = Vec::new(); // per track, the last row it is busy to
    let mut track_of: Vec<(usize, usize)> = Vec::new(); // (source row, track)
    for &(start, end) in &spans {
        let t = match ends.iter().position(|&e| e < start) {
            Some(t) => {
                ends[t] = end;
                t
            }
            None => {
                ends.push(end);
                ends.len() - 1
            }
        };
        track_of.push((start, t));
    }
    for e in &mut edges {
        if nodes[e.from].theme != nodes[e.to].theme {
            e.track = track_of.iter().find(|(s, _)| *s == e.from).map(|&(_, t)| t);
        }
    }
    let cols = nodes.iter().map(|n| n.col + 1).max().unwrap_or(1);
    let tracks = ends.len();
    if cols > MAX_COLS || tracks > MAX_TRACKS {
        return Err(format!(
            "the course needs {cols} columns and {tracks} gutter tracks; the styles (styles/learn.css) place at most \
             {MAX_COLS} and {MAX_TRACKS}: add rules there and raise MAX_COLS or MAX_TRACKS"
        ));
    }
    Ok(Course { themes, nodes, edges, cols, tracks })
}

/// `2026-10-28` as `Oct 28` (`short`) or `October 28, 2026`.
pub fn date(d: &str, short: bool) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (y, m, day) = (
        d.get(..4).unwrap_or(""),
        d.get(5..7).and_then(|m| m.parse::<usize>().ok()).unwrap_or(1),
        d.get(8..10).unwrap_or("").trim_start_matches('0'),
    );
    let month = MONTHS[m.clamp(1, 12) - 1];
    if short { format!("{} {day}", &month[..3]) } else { format!("{month} {day}, {y}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Scalar, Value};

    fn value(fields: &[(&str, Scalar)]) -> Value {
        fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    struct W {
        w: World,
    }

    impl W {
        fn new() -> W {
            let mut w = World::default();
            let site = w.spawn(None, Entity { name: "site".into(), ..Entity::default() }).unwrap();
            for p in ["learn", "updates"] {
                w.spawn(
                    Some(site),
                    Entity { name: p.into(), isa: vec!["Page".into()], ..Entity::default() },
                )
                .unwrap();
            }
            let themes =
                w.spawn(None, Entity { name: "themes".into(), ..Entity::default() }).unwrap();
            for (n, o) in [("basics", 1), ("canary", 2)] {
                let v = value(&[("order", Scalar::Int(o)), ("title", Scalar::Str(n.into()))]);
                w.spawn(
                    Some(themes),
                    Entity {
                        name: n.into(),
                        values: vec![("Theme".into(), v)],
                        ..Entity::default()
                    },
                )
                .unwrap();
            }
            W { w }
        }

        fn node(
            &mut self,
            parent: &str,
            name: &str,
            comp: (&str, Value),
            theme: &str,
            requires: &[&str],
        ) {
            let p = self.w.lookup(parent).unwrap();
            let mut pairs = vec![("InTheme".to_string(), vec![format!("themes.{theme}")])];
            if !requires.is_empty() {
                pairs.push(("Requires".into(), requires.iter().map(|r| r.to_string()).collect()));
            }
            self.w
                .spawn(
                    Some(p),
                    Entity {
                        name: name.into(),
                        isa: vec!["Page".into()],
                        values: vec![(comp.0.into(), comp.1)],
                        pairs,
                        ..Entity::default()
                    },
                )
                .unwrap();
        }

        fn lesson(&mut self, name: &str, order: i64, requires: &[&str]) {
            self.node(
                "site.learn",
                name,
                ("Lesson", value(&[("order", Scalar::Int(order))])),
                "basics",
                requires,
            );
        }

        fn update(&mut self, name: &str, date: &str, theme: &str, requires: &[&str]) {
            let v = value(&[
                ("date", Scalar::Str(date.into())),
                ("version", Scalar::Str("0.20261028.3".into())),
            ]);
            self.node("site.updates", name, ("Update", v), theme, requires);
        }

        fn course(&self) -> Result<Course, String> {
            build(
                &self.w,
                &[self.w.lookup("site.learn").unwrap(), self.w.lookup("site.updates").unwrap()],
            )
        }
    }

    /// The site's own course: a diamond of lessons, an update off lesson 4 in the band, five off the lessons below.
    fn site() -> W {
        let mut w = W::new();
        w.lesson("builds", 1, &[]);
        w.lesson("incremental", 2, &["site.learn.builds"]);
        w.lesson("macros", 3, &["site.learn.builds"]);
        w.lesson("hermetic", 4, &["site.learn.incremental", "site.learn.macros"]);
        w.lesson("sharing", 5, &["site.learn.hermetic"]);
        w.update("pinned", "2026-10-27", "basics", &["site.learn.hermetic"]);
        w.update("why", "2026-10-28", "canary", &["site.learn.incremental"]);
        w.update("tests", "2026-10-28", "canary", &["site.learn.hermetic"]);
        w.update("state", "2026-10-28", "canary", &["site.learn.macros"]);
        w.update("shared", "2026-10-29", "canary", &["site.learn.sharing"]);
        w.update("constm", "2026-10-29", "canary", &["site.learn.macros"]);
        w
    }

    #[test]
    fn lays_out_the_site_course() {
        let c = site().course().unwrap();
        let rows: Vec<(&str, usize)> = c.nodes.iter().map(|n| (n.key.as_str(), n.col)).collect();
        assert_eq!(
            rows,
            [
                ("builds", 0),
                ("incremental", 0),
                // builds -> macros cannot run down column 0 past incremental.
                ("macros", 1),
                ("hermetic", 0),
                // An update branches off the lesson it extends, right after it.
                ("updates/pinned", 1),
                ("sharing", 0),
                // The canary band: no edge inside it, so column 0, in the order of what they extend.
                ("updates/why", 0),
                ("updates/state", 0),
                ("updates/constm", 0),
                ("updates/tests", 0),
                ("updates/shared", 0),
            ]
        );
        assert_eq!(c.cols, 2);
        // incremental, macros, hermetic and sharing each feed the canary band: four overlapping spans.
        assert_eq!(c.tracks, 4);
        let tracks: Vec<(usize, usize, Option<usize>)> =
            c.edges.iter().filter(|e| e.track.is_some()).map(|e| (e.from, e.to, e.track)).collect();
        assert_eq!(
            tracks,
            [(1, 6, Some(0)), (2, 7, Some(1)), (2, 8, Some(1)), (3, 9, Some(2)), (5, 10, Some(3))]
        );
        assert!(
            c.edges
                .iter()
                .filter(|e| e.track.is_none())
                .all(|e| c.nodes[e.from].theme == c.nodes[e.to].theme)
        );
        assert_eq!(c.extends(3), "lessons 2 and 3");
        let newest: Vec<&str> =
            c.updates_newest_first(&site().w).iter().map(|n| n.key.as_str()).collect();
        assert_eq!(newest[..2], ["updates/constm", "updates/shared"]);
        assert_eq!(newest.last(), Some(&"updates/pinned"));
    }

    #[test]
    fn no_edge_shares_a_column_with_a_node_or_another_edge() {
        let c = site().course().unwrap();
        let mut seen: HashSet<(usize, usize)> =
            c.nodes.iter().enumerate().map(|(r, n)| (r, n.col)).collect();
        for e in c.edges.iter().filter(|e| e.track.is_none()) {
            let col = c.nodes[e.to].col;
            for r in e.from + 1..e.to {
                assert!(
                    seen.insert((r, col))
                        || c.edges.iter().any(|o| o.to == e.to && o.from != e.from),
                    "{e:?}"
                );
            }
        }
    }

    #[test]
    fn refuses_cycles_and_later_themes() {
        let mut w = W::new();
        w.lesson("a", 1, &["site.learn.b"]);
        w.lesson("b", 2, &["site.learn.a"]);
        assert!(w.course().unwrap_err().contains("cycle"));

        let mut w = W::new();
        w.lesson("a", 1, &["site.updates.u"]);
        w.update("u", "2026-10-28", "canary", &[]);
        assert!(w.course().unwrap_err().contains("later theme"));

        let mut w = W::new();
        w.lesson("a", 1, &["site.learn.nope"]);
        assert!(w.course().is_err());
    }

    #[test]
    fn dates() {
        assert_eq!(date("2026-10-08", true), "Oct 8");
        assert_eq!(date("2026-10-28", false), "October 28, 2026");
    }
}
