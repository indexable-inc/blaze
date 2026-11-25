//! The terminal against the reference model (model.rs) over drawn streams, and the planted
//! bugs (plant.rs) each caught by the invariant it breaks.
//!
//! After every operation: every cell (char and style) and every row's soft wrap of the
//! scrollback and the screen, the cursor and its pending wrap, and the style refcounts (each
//! interned style's count is the cells naming it plus the cursor's hold). At the end, maybe a
//! resize: with DECAWM the soft-wrapped lines must come through the reflow whole; without, each
//! row cut to the new width.

use core::fmt;
use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};

use crate::model::{LineRow, MCell, MFg, MRow, MStyle, Model, Op, cut_rows, logical_lines};
use crate::plant::VtPlant;
use crate::style::Color;
use crate::terminal::Terminal;
use crate::test_support::{heap, opts};

/// What the terminal got wrong against the model.
#[derive(Clone, PartialEq, Eq, Debug)]
enum VtBroken {
    /// After operation `op`, the cell at `x` of row `row` (scrollback first) differs.
    Cell { op: usize, row: usize, x: usize, want: MCell, got: MCell },
    /// After operation `op`, row `row`'s (soft wrap, wrap continuation) differs.
    Wrap { op: usize, row: usize, want: (bool, bool) },
    /// After operation `op`, the scrollback holds `got` rows, not `want`.
    Rows { op: usize, want: usize, got: usize },
    /// After operation `op`, the cursor (x, y, pending wrap) differs.
    Cursor { op: usize, want: (usize, usize, bool), got: (usize, usize, bool) },
    /// Style `id` holds `refs` references, but `named` cells and the cursor name it.
    StyleRefs { id: u16, refs: u32, named: u32 },
    /// After the resize, line `line` differs.
    Resized { reflow: bool, line: usize, want: Option<String>, got: Option<String> },
}

impl fmt::Display for VtBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VtBroken::Cell { op, row, x, want, got } => {
                write!(f, "after op {op}, row {row} col {x}: wanted {want:?}, got {got:?}")
            }
            VtBroken::Wrap { op, row, want } => {
                write!(f, "after op {op}, row {row}: (wrap, continuation) should be {want:?}")
            }
            VtBroken::Rows { op, want, got } => {
                write!(f, "after op {op}: {got} scrollback rows, wanted {want}")
            }
            VtBroken::Cursor { op, want, got } => {
                write!(f, "after op {op}: cursor (x, y, pending) {got:?}, wanted {want:?}")
            }
            VtBroken::StyleRefs { id, refs, named } => {
                write!(
                    f,
                    "style {id} holds {refs} references but {named} cells and cursors name it"
                )
            }
            VtBroken::Resized { reflow, line, want, got } => {
                write!(
                    f,
                    "after the resize (reflow {reflow}), line {line}: got {got:?}, wanted {want:?}"
                )
            }
        }
    }
}

fn style_of<P: Planted<VtPlant>>(t: &Terminal<'_, P>, id: u16) -> MStyle {
    let st = t.screen().style(id);
    MStyle {
        bold: st.flags.bold,
        italic: st.flags.italic,
        inverse: st.flags.inverse,
        fg: match st.fg {
            Color::None => MFg::None,
            Color::Palette(n) => MFg::Palette(n),
            Color::Rgb(c) => MFg::Rgb(c.r, c.g, c.b),
        },
    }
}

/// Every row of the terminal's screen, scrollback first, as model rows.
fn rows_of<P: Planted<VtPlant>>(t: &Terminal<'_, P>) -> StdVec<MRow> {
    let s = t.screen();
    (0..s.pages.total())
        .map(|r| {
            let cells = s
                .pages
                .cells(r)
                .iter()
                .map(|c| MCell {
                    ch: if c.has_text() { char::from_u32(c.codepoint()) } else { None },
                    style: style_of(t, c.style_id()),
                })
                .collect();
            let row = s.pages.row(r);
            MRow { cells, wrap: row.wrap, cont: row.wrap_continuation }
        })
        .collect()
}

/// The terminal's rows as they read after a reflow: every row is written, a blank row is a
/// blank line, and the cursor adds nothing.
fn plain_rows_of<P: Planted<VtPlant>>(t: &Terminal<'_, P>) -> StdVec<LineRow> {
    rows_of(t)
        .into_iter()
        .map(|r| LineRow { text: r.padded(), wrap: r.wrap, cont: false, cursor: None })
        .collect()
}

fn style_refs<P: Planted<VtPlant>>(t: &Terminal<'_, P>) -> Result<(), Broken<VtBroken>> {
    let s = t.screen();
    let mut named: BTreeMap<u16, u32> = BTreeMap::new();
    for r in 0..s.pages.total() {
        for c in s.pages.cells(r) {
            if c.style_id() != 0 {
                *named.entry(c.style_id()).or_default() += 1;
            }
        }
    }
    if s.cursor.style_id != 0 {
        *named.entry(s.cursor.style_id).or_default() += 1;
    }
    for (&id, &n) in &named {
        let refs = s.styles.refs(id);
        if refs != n {
            return Err(Broken::Oracle(VtBroken::StyleRefs { id, refs, named: n }));
        }
    }
    if s.styles.live() != named.len() {
        return Err(Broken::Harness(std::format!(
            "{} styles live but {} named: a style leaked",
            s.styles.live(),
            named.len()
        )));
    }
    Ok(())
}

fn compare<P: Planted<VtPlant>>(
    op: usize,
    m: &Model,
    t: &Terminal<'_, P>,
) -> Result<(), Broken<VtBroken>> {
    let got = rows_of(t);
    let history = t.screen().pages.active_top();
    if history != m.history.len() || got.len() != m.history.len() + m.rows {
        return Err(Broken::Oracle(VtBroken::Rows { op, want: m.history.len(), got: history }));
    }
    for (row, (want, got)) in m.all_rows().zip(&got).enumerate() {
        for (x, (w, g)) in want.cells.iter().zip(&got.cells).enumerate() {
            if w != g {
                return Err(Broken::Oracle(VtBroken::Cell { op, row, x, want: *w, got: *g }));
            }
        }
        if (want.wrap, want.cont) != (got.wrap, got.cont) {
            return Err(Broken::Oracle(VtBroken::Wrap { op, row, want: (want.wrap, want.cont) }));
        }
    }
    let c = t.screen().cursor;
    let (want, got) = ((m.x, m.y, m.pending_wrap), (c.x, c.y, c.pending_wrap));
    if want != got {
        return Err(Broken::Oracle(VtBroken::Cursor { op, want, got }));
    }
    style_refs(t)
}

fn against_the_model(tape: &Tape, plant: impl Planted<VtPlant>) -> Result<(), Broken<VtBroken>> {
    let cols = tape.int_in(2..=8u16, "cols");
    let rows = tape.int_in(2..=7u16, "rows");
    let ops = tape.vec(0..=40, "ops", |t| Op::draw(t, cols, rows));
    let mut t = Terminal::new_planted(opts(cols, rows), heap(), plant)
        .map_err(|e| Broken::Harness(std::format!("the test heap refused: {e:?}")))?;
    let mut m = Model::new(cols.into(), rows.into());
    let mut bytes = StdVec::new();
    for (i, op) in ops.iter().enumerate() {
        bytes.clear();
        op.bytes(&mut bytes);
        t.feed(&bytes);
        m.apply(op);
        compare(i, &m, &t)?;
    }
    if !tape.coin("resize") {
        return Ok(());
    }
    let to = (tape.int_in(1..=10u16, "to cols"), tape.int_in(1..=8u16, "to rows"));
    let reflow = m.autowrap;
    let want = if reflow && to.0 == cols {
        // The width kept: no reflow, the rows as they are.
        let plain = m.all_rows().map(|r| LineRow {
            text: r.padded(),
            wrap: r.wrap,
            cont: false,
            cursor: None,
        });
        logical_lines(plain)
    } else if reflow {
        logical_lines(m.line_rows().into_iter())
    } else {
        cut_rows(m.all_rows().map(|r| r.padded()), usize::from(to.0))
    };
    t.resize(to.0, to.1);
    let got = if reflow {
        logical_lines(plain_rows_of(&t).into_iter())
    } else {
        cut_rows(rows_of(&t).iter().map(|r| r.padded()), usize::from(to.0))
    };
    if want != got {
        let line = want.iter().zip(&got).take_while(|(a, b)| a == b).count();
        return Err(Broken::Oracle(VtBroken::Resized {
            reflow,
            line,
            want: want.get(line).cloned(),
            got: got.get(line).cloned(),
        }));
    }
    style_refs(&t)
}

/// Found by the model check. A blank row that continues a wrap writes nothing when it reflows
/// (PageList.zig:1593, `if (!src_row.wrap_continuation) self.new_rows += 1`), so the row after it
/// joins the wrapped line. SD keeps the wrap flag on the cleared rows, so they stay blank
/// continuations. A quirk, kept.
#[test]
fn reflow_joins_across_a_blank_wrap_continuation() {
    let mut t = crate::test_support::fed(2, 2, b"aaaaaa\x1b[2T\x08\x1b[1Sa");
    t.resize(1, 1);
    let lines = logical_lines(plain_rows_of(&t).into_iter());
    assert_eq!(lines.first().map(String::as_str), Some("aa  a"), "{lines:?}");
}

#[test]
fn the_terminal_agrees_with_the_model() {
    check(Config::cases(5000), |tape| against_the_model(tape, NoPlant));
}

blaze_sim::plants! {
    /// The bugs planted in the page list, the screen and the terminal, each with the
    /// invariant that must catch it.
    enum SimPlant breaks VtBroken {
        /// The code as it is.
        None,
        /// SU drops the pending wrap: the next print overwrites the last column.
        PendingWrapClearedOnScrollUp => VtBroken::Cell { .. } | VtBroken::Cursor { .. },
        /// Reflow loses the last cell of a soft-wrapped row.
        ReflowDropsWrappedTail => VtBroken::Resized { reflow: true, .. },
        /// Overwriting a cell with its own style drops a reference.
        StyleFreedWhileNamed => VtBroken::StyleRefs { .. },
        /// Swapping the first row of a page with the last row of the page before it swaps
        /// within the earlier page: IL/DL/SU/SD move the wrong rows.
        PageBoundaryOffByOne => VtBroken::Cell { .. } | VtBroken::Wrap { .. },
    }
}

impl SimPlant {
    fn planted(self) -> VtPlant {
        match self {
            SimPlant::None => VtPlant::None,
            SimPlant::PendingWrapClearedOnScrollUp => VtPlant::PendingWrapClearedOnScrollUp,
            SimPlant::ReflowDropsWrappedTail => VtPlant::ReflowDropsWrappedTail,
            SimPlant::StyleFreedWhileNamed => VtPlant::StyleFreedWhileNamed,
            SimPlant::PageBoundaryOffByOne => VtPlant::PageBoundaryOffByOne,
        }
    }
}

#[test]
fn every_vt_plant_is_caught_by_its_invariant() {
    controls(Config::cases(1000), |tape, plant: SimPlant| against_the_model(tape, plant.planted()));
}
