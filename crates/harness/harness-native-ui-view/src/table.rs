//! `ui.Table` (prims.flecs) filled from typed rows: one primitive for every table the app draws (a tool card's result, a
//! markdown table in a reply, the gallery's). Each cell says what it holds ([`Kind`]) and that picks its look, what it
//! shows and how it measures: words in the UI face, inline markdown with its faces and links, code-ish values in the
//! monospace face, numbers at the end in tabular figures, a bool as a mark, a url as its host in the link ink (the whole
//! url on hover, opened on a press), nulls dim.
//!
//! Columns are as wide as they need, measured by the host's typesetter: a short column (a bool, a number, a few words)
//! hugs its widest cell and its header is never cut; the long ones share what is left by how much they need, down to a
//! floor (at least half of what shows across, shared among them, so the main column is never cut to a sliver); past the
//! floors the table scrolls sideways (its `scroll`). A cell cut short shows its whole value on hover.
//! A press on a row (when the host gives an action) opens it below as a record, every field whole and wrapped at the
//! width that shows (Nushell's `get`): it stays in view while the rows scroll sideways under it. Only the rows in view
//! exist, in two lazy columns around the open record.
use std::borrow::Cow;

use harness_native_api::HoverAmt;
use harness_native_flecs::prelude::*;
use harness_native_ui_input_api::{Action, Tooltip};
use harness_native_ui_layout_api::{AlignmentGuide, Flex, Intrinsic, Justify, ScrollState, Size, SizeKind, Sizing};
use harness_native_ui_look_api::TextStyle;
use harness_native_ui_node_api::Laid;

use crate::{View, for_each_of, lazy_for_each, prefab};

/// What a cell holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Words, in the UI face.
    Text,
    /// Inline markdown (a markdown table's cell): bold, italic, code chips, links as their words in the link ink.
    Markdown,
    /// A code-ish value (an id, a path, a nested value), in the monospace face.
    Code,
    /// A number: at the end of its column, in tabular figures.
    Number,
    /// `true` or `false`, shown as a check or a dash.
    Bool,
    /// A url: its host in the link ink, the whole url on hover, opened on a press.
    Url,
    /// Quiet words: a null, an index, a record's keys.
    Dim,
}

/// One cell: what it holds and its value as text (a url whole, a bool `true`/`false`, markdown as written).
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub kind: Kind,
    pub text: String,
}

impl Cell {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }

    /// What the cell shows in its row: a url's host, a bool's mark, the rest as it is.
    #[must_use]
    pub fn shown(&self) -> Cow<'_, str> {
        match self.kind {
            Kind::Url => Cow::Borrowed(host(&self.text)),
            Kind::Bool => Cow::Borrowed(if self.text == "true" { "\u{2713}" } else { "\u{2013}" }),
            _ => Cow::Borrowed(&self.text),
        }
    }

    /// The url a press on it opens: a url cell's own (http or https only).
    #[must_use]
    pub fn link(&self) -> Option<&str> {
        (self.kind == Kind::Url && (self.text.starts_with("https://") || self.text.starts_with("http://"))).then_some(self.text.as_str())
    }

    /// The cell as markdown source, for a copy: markdown as written, anything else with its pipes escaped.
    fn markdown(&self) -> String {
        match self.kind {
            Kind::Markdown => self.text.clone(),
            _ => self.text.replace('|', "\\|").replace('\n', " "),
        }
    }
}

/// A url's host: `https://www.corrosionhour.com/a/b` is `corrosionhour.com`.
#[must_use]
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let h = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let h = h.rsplit_once('@').map_or(h, |(_, h)| h);
    h.strip_prefix("www.").unwrap_or(h)
}

/// Where a column's cells sit: `Auto` puts numbers at the end, marks in the middle and the rest at the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Auto,
    Start,
    Center,
    End,
}

/// A column: its header and where its cells sit.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub align: Align,
}

/// A table's data: its columns, its rows (a short row reads as empty cells), and whether its header shows (a record's
/// key/value rows have none).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub cols: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub head: bool,
}

static EMPTY: Cell = Cell { kind: Kind::Text, text: String::new() };

impl Table {
    /// Row `r`'s cell in column `c` (an empty one past a short row's end).
    #[must_use]
    pub fn cell(&self, r: usize, c: usize) -> &Cell {
        self.rows.get(r).and_then(|row| row.get(c)).unwrap_or(&EMPTY)
    }

    /// Where column `c`'s cells sit, `Auto` resolved from what they hold.
    #[must_use]
    pub fn align(&self, c: usize) -> Align {
        match self.cols[c].align {
            Align::Auto => {
                let kinds = || {
                    self.rows.iter().filter_map(|r| r.get(c)).filter(|x| !(x.text.is_empty() || (x.kind == Kind::Dim && x.text == "null")))
                };
                if kinds().next().is_none() {
                    Align::Start
                } else if kinds().all(|x| x.kind == Kind::Number) {
                    Align::End
                } else if kinds().all(|x| x.kind == Kind::Bool) {
                    Align::Center
                } else {
                    Align::Start
                }
            }
            a => a,
        }
    }

    fn head_markdown(&self) -> String {
        let names: Vec<String> = self.cols.iter().map(|c| c.name.replace('|', "\\|")).collect();
        let rule: Vec<&str> = (0..self.cols.len())
            .map(|c| match self.cols[c].align {
                Align::Start => ":--",
                Align::Center => ":-:",
                Align::End => "--:",
                Align::Auto => "---",
            })
            .collect();
        format!("| {} |\n| {} |", names.join(" | "), rule.join(" | "))
    }

    fn row_line(&self, r: usize) -> String {
        let cells: Vec<String> = (0..self.cols.len()).map(|c| self.cell(r, c).markdown()).collect();
        format!("| {} |", cells.join(" | "))
    }

    /// The table as a GitHub markdown table.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut out = self.head_markdown();
        for r in 0..self.rows.len() {
            out.push('\n');
            out.push_str(&self.row_line(r));
        }
        out
    }

    /// Row `r` under the header, as a markdown table of one row (what a copy of a row gives).
    #[must_use]
    pub fn row_markdown(&self, r: usize) -> String {
        format!("{}\n{}", self.head_markdown(), self.row_line(r))
    }
}

/// How a table is drawn: `look` is the prefix of its prefabs (`ui.Table`: `ui.TableHead`, `ui.TableCell`,
/// `ui.TableCellMd`, `ui.TableCellCode`, `ui.TableCellNum`, `ui.TableCellDim`, `ui.TableCellLink`, `ui.TableCellMark`,
/// `ui.TableField`, `ui.TableFieldMd`; a view gives its own made from them, for its inks), `shown` how many rows show,
/// `action` what a row's press sends (`{action}:{row}`; None: rows do not open) and `open` the row opened below.
#[derive(Clone, Copy, Debug)]
pub struct Opts<'s> {
    pub look: &'s str,
    pub shown: usize,
    pub action: Option<&'s str>,
    pub open: Option<usize>,
}

/// A column at most this many ems wide hugs its widest cell, never cut.
const SHORT_EM: f32 = 11.0;
/// The narrowest a long column gets before the table scrolls sideways.
const FLOOR_EM: f32 = 8.0;
/// How much of what shows across the long columns keep between them before the table scrolls sideways.
const LONG_SHARE: f32 = 0.5;
/// The widest a column grows.
const MAX_EM: f32 = 34.0;
/// Rows measured for widths (a long table's later rows are like its first).
const MEASURED_ROWS: usize = 200;

/// A column's width as layout takes it: Grow, weighted by what it needs, from its floor up to its need.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Width {
    pub need: f32,
    pub floor: f32,
}

impl Width {
    /// The width of a column whose header is `head` points wide and widest cell `widest`, in a face `em` points tall: a
    /// short column's floor is its need (it hugs its content); a long one's is the less of its need and the most of a few
    /// words, its header and `share` (its part of what shows across).
    #[must_use]
    pub fn of(head: f32, widest: f32, em: f32, share: f32) -> Self {
        let need = Self::need(head, widest, em);
        let floor = if Self::short(need, em) { need } else { head.max(FLOOR_EM * em).max(share).min(need) };
        Self { need, floor }
    }

    fn need(head: f32, widest: f32, em: f32) -> f32 {
        head.max(widest).min(MAX_EM * em).max(head).max(1.0)
    }

    fn short(need: f32, em: f32) -> bool {
        need <= SHORT_EM * em
    }

    fn size(self) -> Size {
        Size { w: Sizing { kind: SizeKind::Grow, value: self.need, min: self.floor, max: self.need }, h: Sizing::GROW }
    }
}

/// The prefab of a cell of `kind` in look `look`.
fn cell_prefab(look: &str, kind: Kind) -> String {
    let part = match kind {
        Kind::Text => "Cell",
        Kind::Markdown => "CellMd",
        Kind::Code => "CellCode",
        Kind::Number => "CellNum",
        Kind::Bool => "CellMark",
        Kind::Url => "CellLink",
        Kind::Dim => "CellDim",
    };
    format!("{look}{part}")
}

/// The text style of prefab `name`'s `text` part (or `part`), as a scene wrote it.
fn style_of(w: WorldRef<'_>, name: &str, part: &str) -> TextStyle {
    prefab(w, name).and_then(|p| p.lookup_child(part)).and_then(|t| t.try_cloned::<&TextStyle>()).unwrap_or_default()
}

/// Fill `ui.Table` `t` with `data` as `opts` say, measuring words with `measure` (text, style, inline markdown) in points.
pub fn table<'a>(t: View<'a>, data: &Table, opts: Opts<'_>, measure: &mut dyn FnMut(&str, &TextStyle, bool) -> f32) {
    let Some(scroll) = t.part("scroll") else { return };
    let Some(inner) = scroll.part("inner") else { return };
    let (Some(head_row), Some(rule), Some(body), Some(open_node), Some(rest)) =
        (inner.part("head"), inner.part("rule"), inner.part("body"), inner.part("open"), inner.part("rest"))
    else {
        return;
    };
    let w = t.entity().world();
    let look = opts.look;
    let n = data.cols.len();
    let shown = opts.shown.min(data.rows.len());
    let head_pf = format!("{look}Head");
    let head_style = style_of(w, &head_pf, "text");
    // Each kind's style, once.
    let kinds = [Kind::Text, Kind::Markdown, Kind::Code, Kind::Number, Kind::Bool, Kind::Url, Kind::Dim];
    let styles: Vec<(Kind, String, TextStyle)> = kinds
        .iter()
        .map(|&k| {
            let pf = cell_prefab(look, k);
            let st = style_of(w, &pf, "text");
            (k, pf, st)
        })
        .collect();
    let of = |k: Kind| styles.iter().find(|s| s.0 == k).expect("every kind has a style");
    let em = of(Kind::Text).2.size.max(1.0);
    let gap = head_row.entity().try_cloned::<&Flex>().map_or(0.0, |f| f.gap);
    let pad = head_row.entity().try_cloned::<&harness_native_ui_layout_api::Padding>().map_or(0.0, |p| p.l + p.r);
    // What shows across: the sideways scroll's width as last laid (none before the first layout).
    let view_w = scroll.entity().try_cloned::<&Laid>().map_or(0.0, |l| l.w);
    let measured: Vec<(f32, f32)> = (0..n)
        .map(|c| {
            let head = if data.head { measure(&data.cols[c].name, &head_style, false) } else { 0.0 };
            let widest = (0..shown.min(MEASURED_ROWS))
                .map(|r| {
                    let cell = data.cell(r, c);
                    measure(&cell.shown(), &of(cell.kind).2, cell.kind == Kind::Markdown)
                })
                .fold(0.0, f32::max);
            (head, widest)
        })
        .collect();
    let long = measured.iter().filter(|&&(h, x)| !Width::short(Width::need(h, x, em), em)).count().max(1);
    let share = (view_w - pad).max(0.0) * LONG_SHARE / long as f32;
    let widths: Vec<Width> = measured.iter().map(|&(head, widest)| Width::of(head, widest, em, share)).collect();
    let aligns: Vec<Justify> = (0..n)
        .map(|c| match data.align(c) {
            Align::End => Justify::End,
            Align::Center => Justify::Center,
            Align::Start | Align::Auto => Justify::Start,
        })
        .collect();
    // As wide as the room, never narrower than every column's floor: past that the table scrolls sideways.
    let least = widths.iter().map(|w| w.floor).sum::<f32>() + gap * n.saturating_sub(1) as f32 + pad;
    set_size(inner, Size { w: Sizing { kind: SizeKind::Grow, value: 1.0, min: least, max: 0.0 }, h: Sizing::FIT });
    let scroll_x = keep_scroll(scroll, inner);
    let shape = |v: View<'_>, c: usize| {
        set_size(v, widths[c].size());
        let mut f = v.entity().try_cloned::<&Flex>().unwrap_or_default();
        if f.justify != aligns[c] {
            f.justify = aligns[c];
            v.entity().set(f);
        }
    };
    let row_h = body.entity().try_cloned::<&harness_native_ui_layout_api::Lazy>().map_or(22.0, |l| l.row);
    if data.head {
        set_size(head_row, Size { w: Sizing::GROW, h: Sizing::fixed(row_h) });
        set_size(rule, Size { w: Sizing::GROW, h: Sizing::fixed(1.0) });
    } else {
        set_size(head_row, Size { w: Sizing::GROW, h: Sizing::fixed(0.0) });
        set_size(rule, Size { w: Sizing::GROW, h: Sizing::fixed(0.0) });
    }
    let heads = (0..if data.head { n } else { 0 }).map(|c| (format!("c{c}"), head_pf.as_str(), c));
    for_each_of(head_row, heads, |v, c, _| {
        if let Some(x) = v.part("text") {
            x.text(&data.cols[c].name);
        }
        shape(v, c);
    });
    let open = opts.open.filter(|&o| o < shown);
    let fill_row = |row: View<'_>, r: usize, fresh: bool| {
        if fresh {
            row.set(HoverAmt::default());
        }
        let act = opts.action.map(|a| format!("{a}:{r}")).unwrap_or_default();
        if row.entity().try_cloned::<&Action>().is_none_or(|x| x.name.as_str() != act) {
            row.set(Action { name: act.as_str().into() });
        }
        let cells = (0..n).map(|c| (format!("c{c}"), of(data.cell(r, c).kind).1.as_str(), c));
        for_each_of(row, cells, |v, c, _| {
            let cell = data.cell(r, c);
            let shown = cell.shown();
            let text = v.part("text");
            if let Some(x) = text {
                x.text(&shown);
            }
            shape(v, c);
            if let Some(url) = cell.link() {
                v.action(&format!("open:{url}"));
            }
            // The whole value on hover: a url's, else the words when the column cuts them.
            let cut = text.is_some_and(|x| {
                let e = x.entity();
                match (e.try_cloned::<&Intrinsic>(), e.try_cloned::<&Laid>()) {
                    (Some(i), Some(l)) => l.w > 0.0 && i.w > l.w + 0.5,
                    _ => false,
                }
            });
            let tip = match cell.kind {
                Kind::Url => Some(cell.text.clone()),
                _ if cut => Some(cell.text.clone()),
                _ => None,
            };
            let had = v.entity().try_cloned::<&Tooltip>();
            match tip {
                Some(s) if had.as_ref().is_none_or(|h| h.text.as_str() != s) => {
                    v.set(Tooltip { text: s.as_str().into() });
                }
                None if had.is_some() => {
                    v.entity().remove(Tooltip::id());
                }
                _ => {}
            }
        });
    };
    let first = open.map_or(shown, |o| o + 1);
    lazy_for_each(body, "ui.TableRow", first as u32, |i| format!("r{i}"), |row, i, fresh| fill_row(row, i as usize, fresh));
    lazy_for_each(rest, "ui.TableRow", (shown - first) as u32, |i| format!("r{}", first + i as usize), |row, i, fresh| {
        fill_row(row, first + i as usize, fresh);
    });
    // The open row as a record below it: each field's name, then its value whole, wrapped.
    let key_style = style_of(w, &format!("{look}Field"), "key");
    let key_w = (0..n).map(|c| measure(&data.cols[c].name, &key_style, false)).fold(0.0, f32::max).min(14.0 * em);
    let (field, field_md) = (format!("{look}Field"), format!("{look}FieldMd"));
    let fields: Vec<(String, &str, (usize, usize))> = open
        .into_iter()
        .flat_map(|o| (0..n).map(move |c| (o, c)))
        .map(|(o, c)| {
            let pf = if data.cell(o, c).kind == Kind::Markdown { field_md.as_str() } else { field.as_str() };
            (format!("f{c}"), pf, (o, c))
        })
        .collect();
    // As wide as what shows across, not the rows' whole width, and held in view as they scroll sideways under it: its words
    // wrap where they can be read.
    let open_w = if view_w > 0.0 { Sizing::fixed(view_w) } else { Sizing::GROW };
    set_size(open_node, Size { w: open_w, h: if open.is_some() { Sizing::FIT } else { Sizing::fixed(0.0) } });
    let pin = AlignmentGuide { dx: -scroll_x, dy: 0.0 };
    if open_node.entity().try_cloned::<&AlignmentGuide>().unwrap_or_default() != pin {
        open_node.set(pin);
    }
    for_each_of(open_node, fields, |v, (o, c), _| {
        if let Some(k) = v.part("key") {
            k.text(&data.cols[c].name);
            set_size(k, Size { w: Sizing::fixed(key_w), h: Sizing::FIT });
        }
        if let Some(x) = v.part("value") {
            let cell = data.cell(o, c);
            x.text(if cell.text.is_empty() { "\u{2014}" } else { &cell.text });
        }
    });
}

fn set_size(v: View<'_>, want: Size) {
    if v.entity().try_cloned::<&Size>() != Some(want) {
        v.set(want);
    }
}

/// Keep a table's sideways scroll inside what its content overhangs (none when it fits); the offset it keeps.
fn keep_scroll(scroll: View<'_>, inner: View<'_>) -> f32 {
    let Some(s) = scroll.entity().try_cloned::<&ScrollState>() else { return 0.0 };
    let Some(l) = scroll.entity().try_cloned::<&Laid>() else { return s.x };
    let content = inner.entity().try_cloned::<&Laid>().map_or(0.0, |i| i.w);
    let x = s.x.clamp(0.0, (content - l.w).max(0.0));
    if (x - s.x).abs() > 0.01 {
        scroll.set(ScrollState { x, y: s.y });
    }
    x
}

/// The table `e` is in (a node of it, or the table itself), by its parts: the `ui.Table` node, its `scroll` and `inner`.
fn table_of(e: EntityView<'_>) -> Option<EntityView<'_>> {
    std::iter::successors(Some(e), |n| n.parent()).take(12).find(|n| {
        n.lookup_child("scroll").and_then(|s| s.lookup_child("inner")).is_some_and(|i| i.lookup_child("body").is_some())
    })
}

/// The table row node `e` is in (a cell, its words, the row itself): its table and its row's index.
#[must_use]
pub fn row_under(e: EntityView<'_>) -> Option<(EntityView<'_>, usize)> {
    let row = std::iter::successors(Some(e), |n| n.parent()).take(6).find(|n| {
        n.parent().is_some_and(|p| matches!(p.name().as_str(), "body" | "rest") && p.parent().is_some_and(|i| i.name() == "inner"))
    })?;
    let r = row.name().strip_prefix('r')?.parse().ok()?;
    Some((table_of(row)?, r))
}

/// Row `r`'s node of the first table at or under `e` (a view holding a table: a chat's transcript row), if it is made (only
/// the rows in view are): what a copy of that row pulses.
#[must_use]
pub fn row_node(e: EntityView<'_>, r: usize) -> Option<EntityView<'_>> {
    let name = format!("r{r}");
    let mut stack = vec![e]; // alloc: a walk down one transcript row's few nodes
    let mut seen = 0;
    while let Some(n) = stack.pop() {
        seen += 1;
        if seen > 4096 {
            return None;
        }
        if let Some(inner) = n.lookup_child("scroll").and_then(|s| s.lookup_child("inner")) {
            return ["body", "rest"].iter().find_map(|part| inner.lookup_child(part).and_then(|p| p.lookup_child(&name)));
        }
        n.each_child(|c| stack.push(c));
    }
    None
}

/// Scroll the table `e` is in sideways by `dx` points (a horizontal swipe over it). Whether it took it: a table whose
/// content overhangs its room.
pub fn scroll_sideways(e: EntityView<'_>, dx: f32) -> bool {
    let Some(t) = table_of(e) else { return false };
    let Some(scroll) = t.lookup_child("scroll") else { return false };
    let Some(inner) = scroll.lookup_child("inner") else { return false };
    let (Some(l), Some(i)) = (scroll.try_cloned::<&Laid>(), inner.try_cloned::<&Laid>()) else { return false };
    let room = (i.w - l.w).max(0.0);
    if room <= 0.5 {
        return false;
    }
    let s = scroll.try_cloned::<&ScrollState>().unwrap_or_default();
    let x = (s.x + dx).clamp(0.0, room);
    if (x - s.x).abs() > 0.01 {
        scroll.set(ScrollState { x, y: s.y });
    }
    true
}

/// One search result as `ui.ResultList` shows it: its title (a link to `url`), where it is (`host`), and a snippet.
#[derive(Clone, Debug, PartialEq)]
pub struct Result {
    pub title: String,
    pub url: String,
    pub host: String,
    pub snippet: String,
}

/// Fill a `ui.ResultList` with `items` in `item` prefabs (`ui.Result` or one made from it): each a link titled as the
/// page is, opening its url (the whole url on hover), its host muted, then the snippet's first two lines.
pub fn results(list: View<'_>, items: &[Result], item: &str) {
    crate::for_each(list, item, items.iter().enumerate().map(|(i, r)| (format!("r{i}"), r)), |v, r, fresh| {
        if let Some(t) = v.part("title") {
            if fresh {
                t.set(HoverAmt::default());
            }
            t.text(if r.title.is_empty() { &r.url } else { &r.title });
            let open = if r.url.starts_with("https://") || r.url.starts_with("http://") { format!("open:{}", r.url) } else { String::new() };
            t.action(&open);
            if t.entity().try_cloned::<&Tooltip>().is_none_or(|x| x.text.as_str() != r.url) {
                t.set(Tooltip { text: r.url.as_str().into() });
            }
        }
        if let Some(h) = v.part("host") {
            h.text(&r.host);
        }
        if let Some(s) = v.part("snippet") {
            s.text(&r.snippet);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_columns_hug_and_long_ones_share() {
        // A bool column: its header wider than its marks; never cut.
        assert_eq!(Width::of(40.0, 10.0, 13.0, 200.0), Width { need: 40.0, floor: 40.0 });
        // A long text column: needs its widest cell up to the cap, gives way down to a few words.
        let w = Width::of(30.0, 900.0, 13.0, 0.0);
        assert_eq!(w.need, 34.0 * 13.0);
        assert_eq!(w.floor, 8.0 * 13.0);
        // ...or to its share of what shows, when that is more (never past its need).
        assert_eq!(Width::of(30.0, 900.0, 13.0, 200.0).floor, 200.0);
        assert_eq!(Width::of(30.0, 900.0, 13.0, 1000.0).floor, 34.0 * 13.0);
        assert_eq!(host("https://www.corrosionhour.com/rust-november-2024/?x=1"), "corrosionhour.com");
        assert_eq!(host("http://user@docs.rs:8080/a"), "docs.rs:8080");
    }

    #[test]
    fn a_row_copies_as_markdown() {
        let t = Table {
            cols: vec![Column { name: "a|b".into(), align: Align::End }, Column { name: "c".into(), align: Align::Auto }],
            rows: vec![vec![Cell::new(Kind::Number, "1"), Cell::new(Kind::Markdown, "**x** \\| y")], vec![Cell::new(Kind::Text, "p|q")]],
            head: true,
        };
        assert_eq!(t.row_markdown(0), "| a\\|b | c |\n| --: | --- |\n| 1 | **x** \\| y |");
        assert_eq!(t.row_markdown(1), "| a\\|b | c |\n| --: | --- |\n| p\\|q |  |");
        assert_eq!(t.align(0), Align::End);
    }
}
