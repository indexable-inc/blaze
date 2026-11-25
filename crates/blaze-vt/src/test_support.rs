//! What the terminal tests share: a terminal on the test heap, the screen as text (ghostty's
//! `plainString`), and table-driven cases (name, ghostty's file:line, bytes, expected text and
//! cursor).

use std::string::String;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_own::planted::Planted;

use crate::page::{Cell, Wide};
use crate::plant::VtPlant;
use crate::screen::Screen;
use crate::terminal::{Opts, Terminal};

pub(crate) fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// Rows in a page in the tests: small, so page boundaries are crossed.
pub(crate) const TEST_PAGE_ROWS: usize = 3;

pub(crate) fn opts(cols: u16, rows: u16) -> Opts<'static> {
    let mut o = Opts::new(cols, rows, 1 << 20);
    o.page_rows = TEST_PAGE_ROWS;
    o
}

/// A `cols` × `rows` terminal with a 1 MiB scrollback.
pub(crate) fn term(cols: u16, rows: u16) -> Terminal<'static> {
    Terminal::new_in(opts(cols, rows), heap()).expect("the test heap")
}

/// A terminal fed `bytes`.
pub(crate) fn fed(cols: u16, rows: u16, bytes: &[u8]) -> Terminal<'static> {
    let mut t = term(cols, rows);
    t.feed(bytes);
    t
}

fn row_string<P: Planted<VtPlant>>(s: &Screen<'_, P>, r: usize) -> String {
    let mut out = String::new();
    s.row_text(r, &mut out).expect("a string takes any text");
    out
}

/// Page-list rows `from..to` as text: each row trimmed of trailing blanks, joined by `\n`,
/// trailing blank rows dropped (ghostty's `dumpString` with `unwrap = false`).
fn rows_text<P: Planted<VtPlant>>(s: &Screen<'_, P>, from: usize, to: usize) -> String {
    let mut lines: StdVec<String> = (from..to).map(|r| row_string(s, r)).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// The active area of the screen shown, as text (ghostty's `plainString`).
pub(crate) fn text<P: Planted<VtPlant>>(t: &Terminal<'_, P>) -> String {
    let s = t.screen();
    rows_text(s, s.pages.active_top(), s.pages.total())
}

/// Scrollback and active area, as text.
pub(crate) fn all_text<P: Planted<VtPlant>>(t: &Terminal<'_, P>) -> String {
    let s = t.screen();
    rows_text(s, 0, s.pages.total())
}

/// The scrollback alone, as text.
pub(crate) fn history_text<P: Planted<VtPlant>>(t: &Terminal<'_, P>) -> String {
    let s = t.screen();
    rows_text(s, 0, s.pages.active_top())
}

pub(crate) fn cursor<P: Planted<VtPlant>>(t: &Terminal<'_, P>) -> (usize, usize) {
    let c = t.screen().cursor;
    (c.x, c.y)
}

/// The cell at column `x` of active row `y`.
pub(crate) fn cell<P: Planted<VtPlant>>(t: &Terminal<'_, P>, x: usize, y: usize) -> Cell {
    let s = t.screen();
    s.pages.cell(s.r(y), x)
}

pub(crate) fn wide_at<P: Planted<VtPlant>>(t: &Terminal<'_, P>, x: usize, y: usize) -> Wide {
    cell(t, x, y).wide()
}

/// The replies taken, as a byte vector.
pub(crate) fn replies<P: Planted<VtPlant>>(t: &mut Terminal<'_, P>) -> StdVec<u8> {
    t.take_replies().as_slice().to_vec()
}

/// One table-driven case: a terminal of `cols` × `rows` fed `input` shows `text` with the
/// cursor at `cursor` (x, y).
pub(crate) struct Case {
    pub name: &'static str,
    /// ghostty's file:line of the test this ports.
    pub at: &'static str,
    pub cols: u16,
    pub rows: u16,
    pub input: &'static [u8],
    pub text: &'static str,
    pub cursor: (usize, usize),
}

/// Runs every case, reporting every failure at once.
pub(crate) fn run_cases(cases: &[Case]) {
    let mut failed = StdVec::new();
    for c in cases {
        let t = fed(c.cols, c.rows, c.input);
        let got = text(&t);
        let at = cursor(&t);
        if got != c.text || at != c.cursor {
            failed.push(std::format!(
                "{} ({}):\n  text   {:?}\n  wanted {:?}\n  cursor {:?}, wanted {:?}",
                c.name,
                c.at,
                got,
                c.text,
                at,
                c.cursor
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} cases failed:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}
