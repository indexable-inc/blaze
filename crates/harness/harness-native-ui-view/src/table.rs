//! `ui.Table` (prims.flecs) filled from typed rows: one primitive for every table the app draws (a tool card's result, a
//! markdown table in a reply, the gallery's). Each cell says what it holds ([`Kind`]) and that picks its look, what it
//! shows and how it measures: words in the UI face, inline markdown with its faces and links, code-ish values in the
//! monospace face, numbers at the end in tabular figures, a bool as a mark, a url as its host in the link ink (the whole
//! url on hover, opened on a press), nulls dim.
//!
//! Columns are as wide as they need, measured by the host's typesetter: a short column (a bool, a number, a few words)
//! hugs its widest cell and its header is never cut; the long ones split what the short ones leave ([`allot`]: a column
//! that needs less than an even split keeps only its need, the widest gets the slack), down to a floor of a few words so
//! the main column is never cut to a sliver; only past the floors does the table scroll sideways (its `scroll`). A long
//! cell is cut at its column's end with an ellipsis and shows its whole value on hover.
//! A small table too wide for its room is not cut at all: it is stacked ([`stacks`]), each row a record of its own, its
//! first cell the title and every other field under it by name, whole and wrapped, a hairline between rows; so a
//! comparison of a few things reads in a narrow pane as well as a wide one.
//! A press on a row (when the host gives an action) opens it below as a record, every field whole and wrapped at the
//! width that shows (Nushell's `get`): it stays in view while the rows scroll sideways under it. Only the rows in view
//! exist, in two lazy columns around the open record.
use std::borrow::Cow;

use harness_native_api::HoverAmt;
use harness_native_flecs::prelude::*;
use harness_native_ui_input_api::{Action, Tooltip};
use harness_native_ui_layout_api::{AlignmentGuide, Flex, Intrinsic, Justify, ScrollState, Size, SizeKind, Sizing};
use harness_native_ui_look_api::TextStyle;
use harness_native_ui_text_api::TextRange;
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

/// One cell: what it holds, its value as text (a url whole, a bool `true`/`false`, markdown as written), and what a press
/// on it sends (an action name; a url cell opens its url without one).
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub kind: Kind,
    pub text: String,
    pub press: Option<String>,
}

impl Cell {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into(), press: None }
    }

    /// The cell sending `action` on a press, drawn as a link.
    #[must_use]
    pub fn with_press(mut self, action: impl Into<String>) -> Self {
        self.press = Some(action.into());
        self
    }

    /// What a press on it does: a url cell opens its url, else its own action.
    #[must_use]
    pub fn action(&self) -> Option<Cow<'_, str>> {
        self.link().map(|u| Cow::Owned(format!("open:{u}"))).or_else(|| self.press.as_deref().map(Cow::Borrowed))
    }

    /// What the cell shows in its row: a url's host, a bool's mark, the rest as it is.
    #[must_use]
    pub fn shown(&self) -> Cow<'_, str> {
        match self.kind {
            Kind::Url => Cow::Borrowed(host(&self.text)),
            Kind::Bool => Cow::Borrowed(if self.text == "true" { "\u{2713}" } else { "\u{2013}" }),
            // One line in its row: a value of several shows its first (`OneLine` never breaks).
            _ => harness_native_ui_text_api::one_line(&self.text),
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

static EMPTY: Cell = Cell { kind: Kind::Text, text: String::new(), press: None };

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
/// `action` what a row's press sends (`{action}:{row}`; None: rows do not open), `presses` each row's own press in its place
/// (row `r` sends `presses[r]`), `open` the row opened below, and `find` words to mark in every cell they are in (in the
/// look's `Match` prefab; none when blank).
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts<'s> {
    pub look: &'s str,
    pub shown: usize,
    pub action: Option<&'s str>,
    pub presses: Option<&'s [String]>,
    pub open: Option<usize>,
    pub find: &'s str,
}

/// Where `query` first is in `text`, ignoring case, as UTF-16 units `from..to` of `text` (what a `TextRange` counts). None
/// when it is blank or not there.
#[must_use]
pub fn find(text: &str, query: &str) -> Option<(u32, u32)> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect(); // alloc: one query's few chars
    if q.iter().all(|c| c.is_whitespace()) {
        return None;
    }
    // Each char of `text` lowered (a char may lower to several), with where it starts in UTF-16 units.
    let mut low: Vec<(char, u32)> = Vec::new(); // alloc: one cell's chars
    let mut at = 0u32;
    for ch in text.chars() {
        for l in ch.to_lowercase() {
            low.push((l, at));
        }
        at += ch.len_utf16() as u32;
    }
    let end_of = |k: usize| low.get(k).map_or(at, |&(_, u)| u);
    (0..=low.len().checked_sub(q.len())?)
        .find(|&k| low[k..k + q.len()].iter().map(|&(c, _)| c).eq(q.iter().copied()))
        .map(|k| (low[k].1, end_of(k + q.len())))
}

/// A column at most this many ems wide hugs its widest cell, never cut.
const SHORT_EM: f32 = 11.0;
/// The narrowest a long column gets before the table scrolls sideways.
const FLOOR_EM: f32 = 8.0;
/// The most rows a table stacks ([`stacks`]): past it, it is data to scan down columns, and stays a grid that scrolls.
const STACK_ROWS: usize = 12;
/// How far past its room a table's whole width runs before it stacks: under it, a cell loses a word and the grid stays.
const STACK_OVER: f32 = 1.12;

/// Whether a table of `cols` columns and `rows` rows that needs `whole` points to show every cell whole stacks in a room
/// `room` wide (0: not laid out yet, so not known): it is small, and too wide by more than a word.
#[must_use]
pub fn stacks(cols: usize, rows: usize, whole: f32, room: f32) -> bool {
    room > 0.0 && cols >= 2 && rows <= STACK_ROWS && whole > room * STACK_OVER
}

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
    /// words, its header and `share` (what [`allot`] gave it of the room the short columns leave).
    #[must_use]
    pub fn of(head: f32, widest: f32, em: f32, share: f32) -> Self {
        let need = Self::need(head, widest, em);
        let floor = if Self::short(need, em) { need } else { head.max(FLOOR_EM * em).max(share).min(need) };
        Self { need, floor }
    }

    pub fn need(head: f32, widest: f32, em: f32) -> f32 {
        head.max(widest).min(MAX_EM * em).max(head).max(1.0)
    }

    pub fn short(need: f32, em: f32) -> bool {
        need <= SHORT_EM * em
    }

    fn size(self) -> Size {
        Size { w: Sizing { kind: SizeKind::Grow, value: self.need, min: self.floor, max: self.need }, h: Sizing::GROW }
    }
}

/// Split `room` among columns needing `needs` (in any order): each gets an even share of what is left, or its need when
/// that is less, so what a narrow column does not use goes to the wider ones and the widest takes the slack. Never below
/// zero; the shares sum to `room` unless every column fits whole.
#[must_use]
pub fn allot(needs: &[f32], room: f32) -> Vec<f32> {
    let mut order: Vec<usize> = (0..needs.len()).collect(); // alloc: one table's few columns
    order.sort_by(|&a, &b| needs[a].total_cmp(&needs[b]).then(a.cmp(&b)));
    let mut out = vec![0.0; needs.len()]; // alloc: one table's few columns
    let mut left = room.max(0.0);
    for (k, &c) in order.iter().enumerate() {
        let give = needs[c].min(left / (order.len() - k) as f32);
        out[c] = give;
        left -= give;
    }
    out
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

/// The prefab of `cell` in look `look`: a cell with a press of its own reads as a link.
fn prefab_of(look: &str, cell: &Cell) -> String {
    cell_prefab(look, if cell.press.is_some() && cell.kind != Kind::Markdown { Kind::Url } else { cell.kind })
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
    // The long columns split what the short ones and the gaps leave of what shows across.
    let needs: Vec<f32> = measured.iter().map(|&(h, x)| Width::need(h, x, em)).collect();
    let long: Vec<usize> = (0..n).filter(|&c| !Width::short(needs[c], em)).collect();
    let short_w: f32 = (0..n).filter(|c| !long.contains(c)).map(|c| needs[c]).sum();
    let left = view_w - pad - gap * n.saturating_sub(1) as f32 - short_w;
    let shares = allot(&long.iter().map(|&c| needs[c]).collect::<Vec<_>>(), left);
    let widths: Vec<Width> = (0..n)
        .map(|c| {
            let share = long.iter().position(|&l| l == c).map_or(0.0, |k| shares[k]);
            Width::of(measured[c].0, measured[c].1, em, share)
        })
        .collect();
    // Stacked: every row a record, nothing cut (the module's doc). The grid's parts are then empty and no height.
    let stack = t.part("stack");
    let whole = needs.iter().sum::<f32>() + gap * n.saturating_sub(1) as f32 + pad;
    let stacked = stack.is_some() && stacks(n, shown, whole, view_w);
    set_size(scroll, Size { w: Sizing::GROW, h: if stacked { Sizing::fixed(0.0) } else { Sizing::FIT } });
    let key_style = style_of(w, &format!("{look}Field"), "key");
    let key_w = (0..n).map(|c| measure(&data.cols[c].name, &key_style, false)).fold(0.0, f32::max).min(14.0 * em);
    let (field, field_md) = (format!("{look}Field"), format!("{look}FieldMd"));
    if let Some(stack) = stack {
        let rows = (0..if stacked { shown } else { 0 }).flat_map(|r| {
            let md = n > 0 && data.cell(r, 0).kind == Kind::Markdown;
            let rule = (r > 0).then(|| (format!("d{r}"), "ui.TableStackRule", (r, false)));
            rule.into_iter().chain(std::iter::once((format!("s{r}"), if md { "ui.TableStackRowMd" } else { "ui.TableStackRow" }, (r, true))))
        });
        for_each_of(stack, rows, |v, (r, row), _| {
            if !row {
                return;
            }
            let act = match opts.presses {
                Some(p) => p.get(r).cloned().unwrap_or_default(),
                None => String::new(),
            };
            if v.entity().try_cloned::<&Action>().is_none_or(|x| x.name.as_str() != act) {
                v.set(Action { name: act.as_str().into() });
            }
            if let Some(x) = v.part("title") {
                let cell = data.cell(r, 0);
                x.text(if cell.text.is_empty() { "\u{2014}" } else { &cell.text });
            }
            let Some(fields) = v.part("fields") else { return };
            let each = (1..n).map(|c| {
                let pf = if data.cell(r, c).kind == Kind::Markdown { field_md.as_str() } else { field.as_str() };
                (format!("f{c}"), pf, c)
            });
            for_each_of(fields, each, |f, c, _| {
                if let Some(k) = f.part("key") {
                    k.text(&data.cols[c].name);
                    set_size(k, Size { w: Sizing::fixed(key_w), h: Sizing::FIT });
                }
                if let Some(x) = f.part("value") {
                    let cell = data.cell(r, c);
                    x.text(if cell.text.is_empty() { "\u{2014}" } else { &cell.text });
                }
            });
        });
    }
    let shown = if stacked { 0 } else { shown };
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
    if data.head && !stacked {
        set_size(head_row, Size { w: Sizing::GROW, h: Sizing::fixed(row_h) });
        set_size(rule, Size { w: Sizing::GROW, h: Sizing::fixed(1.0) });
    } else {
        set_size(head_row, Size { w: Sizing::GROW, h: Sizing::fixed(0.0) });
        set_size(rule, Size { w: Sizing::GROW, h: Sizing::fixed(0.0) });
    }
    let heads = (0..if data.head && !stacked { n } else { 0 }).map(|c| (format!("c{c}"), head_pf.as_str(), c));
    for_each_of(head_row, heads, |v, c, _| {
        if let Some(x) = v.part("text") {
            x.text(&data.cols[c].name);
        }
        shape(v, c);
    });
    let open = opts.open.filter(|&o| o < shown);
    let match_pf = format!("{look}Match");
    let fill_row = |row: View<'_>, r: usize, fresh: bool| {
        if fresh {
            row.set(HoverAmt::default());
        }
        let act = match opts.presses {
            Some(p) => p.get(r).cloned().unwrap_or_default(),
            None => opts.action.map(|a| format!("{a}:{r}")).unwrap_or_default(),
        };
        if row.entity().try_cloned::<&Action>().is_none_or(|x| x.name.as_str() != act) {
            row.set(Action { name: act.as_str().into() });
        }
        let cells = (0..n).map(|c| (format!("c{c}"), prefab_of(look, data.cell(r, c)), c));
        let cells: Vec<(String, String, usize)> = cells.collect(); // alloc: one row's few cells
        for_each_of(row, cells.iter().map(|(k, p, c)| (k.as_str(), p.as_str(), *c)), |v, c, _| {
            let cell = data.cell(r, c);
            let shown = cell.shown();
            let text = v.part("text");
            if let Some(x) = text {
                x.text(&shown);
                // The words found, marked under them (markdown's glyphs are not its source's: not marked).
                let hit = if cell.kind == Kind::Markdown { None } else { find(&shown, opts.find) };
                for_each_of(x, hit.map(|h| ("match", match_pf.as_str(), h)), |m, (from, to), fresh| {
                    // It stands on its parent's glyphs (the painter plates them), not in the layout: no `Node`.
                    if fresh {
                        m.entity().remove(harness_native_ui_node_api::Node::id());
                    }
                    let want = TextRange { from, to };
                    if m.entity().try_cloned::<&TextRange>() != Some(want) {
                        m.set(want);
                    }
                });
            }
            shape(v, c);
            if let Some(a) = cell.action() {
                v.action(&a);
            }
            // The whole value on hover: a url's, else the words when the column cuts them.
            let cut = text.is_some_and(|x| {
                let e = x.entity();
                match (e.try_cloned::<&Intrinsic>(), e.try_cloned::<&Laid>()) {
                    (Some(i), Some(l)) => l.w > 0.0 && i.w > l.w + 0.5,
                    _ => false,
                }
            });
            // A value of several lines shows only its first ([`Cell::shown`]): cut too.
            let tip = match cell.kind {
                Kind::Url => Some(cell.text.clone()),
                _ if cut || shown != cell.text.as_str() => Some(cell.text.clone()),
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
        // The room the short columns leave: a narrow long column keeps its need, the widest takes the rest.
        assert_eq!(allot(&[400.0, 150.0], 300.0), vec![150.0, 150.0]);
        assert_eq!(allot(&[400.0, 100.0], 300.0), vec![200.0, 100.0]);
        assert_eq!(allot(&[400.0], -20.0), vec![0.0]);
        assert_eq!(host("https://www.corrosionhour.com/rust-november-2024/?x=1"), "corrosionhour.com");
        assert_eq!(host("http://user@docs.rs:8080/a"), "docs.rs:8080");
        // A find ignores case and counts UTF-16 units.
        assert_eq!(find("Hello World", "world"), Some((6, 11)));
        assert_eq!(find("\u{1F600}ab", "B"), Some((3, 4)));
        assert_eq!(find("abc", " "), None);
        assert_eq!(find("ab", "abc"), None);
    }

    #[test]
    fn a_small_table_too_wide_stacks() {
        // Five columns of words in a narrow pane: stacked. The same in a wide one, a long one, or one a word over: a grid.
        assert!(stacks(5, 4, 1400.0, 700.0));
        assert!(!stacks(5, 4, 1400.0, 1500.0));
        assert!(!stacks(5, 400, 1400.0, 700.0));
        assert!(!stacks(5, 4, 740.0, 700.0));
        assert!(!stacks(5, 4, 1400.0, 0.0));
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
