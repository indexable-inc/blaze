//! The reference model the simulation tests hold the terminal to: a grid of plain cells (a
//! char or nothing, and a small style) in a `Vec` of rows, scrollback a `Vec` of rows, no pages,
//! no interning, no out-of-line storage. It covers a subset of the terminal on the primary
//! screen with no left/right margins and origin off: printing ASCII, CR/LF/BS, CUP and the
//! relative moves, IND/RI, SU/SD, IL/DL, ICH/DCH/ECH, EL, ED, DECSTBM, DECAWM and SGR 0, 1, 7,
//! 22, 27, 30-37, 39. Each operation is written from ghostty's Terminal.zig as plainly as it
//! can be, the row-flag quirks included (named where they are).

use std::string::String;
use std::vec::Vec as StdVec;

use blaze_sim::Tape;

/// The style subset the model tracks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct MStyle {
    pub bold: bool,
    pub italic: bool,
    pub inverse: bool,
    pub fg: MFg,
}

/// The foreground the model tracks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum MFg {
    #[default]
    None,
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// A cell: `None` is an empty cell (ghostty's codepoint 0).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct MCell {
    pub ch: Option<char>,
    pub style: MStyle,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct MRow {
    pub cells: StdVec<MCell>,
    pub wrap: bool,
    /// The row continues a soft-wrapped row (ghostty's `wrap_continuation`).
    pub cont: bool,
}

impl MRow {
    fn blank(cols: usize) -> MRow {
        MRow { cells: std::vec![MCell::default(); cols], wrap: false, cont: false }
    }

    fn unwrap_flags(&mut self) {
        self.wrap = false;
        self.cont = false;
    }

    /// The row as text, empty cells as spaces, untrimmed.
    pub(crate) fn padded(&self) -> String {
        padded(&self.cells)
    }
}

/// Cells as text, empty cells as spaces.
pub(crate) fn padded(cells: &[MCell]) -> String {
    cells.iter().map(|c| c.ch.unwrap_or(' ')).collect()
}

/// Cells as text, empty cells as `\0` (a printed space is not empty).
pub(crate) fn raw(cells: &[MCell]) -> String {
    cells.iter().map(|c| c.ch.unwrap_or('\0')).collect()
}

impl Model {
    /// The rows as a reflow reads them.
    pub(crate) fn line_rows(&self) -> StdVec<LineRow> {
        let cursor_row = self.history.len() + self.y;
        self.all_rows()
            .enumerate()
            .map(|(i, r)| LineRow {
                text: raw(&r.cells),
                wrap: r.wrap,
                cont: r.cont,
                cursor: (i == cursor_row).then_some(self.x),
            })
            .collect()
    }
}

/// One operation of a drawn stream: what the model applies and the bytes the terminal is fed.
#[derive(Clone, Debug)]
pub(crate) enum Op {
    Print(String),
    Cr,
    Lf,
    Bs,
    /// CUP row;col, 1-based as sent.
    Cup(u16, u16),
    Cuu(u16),
    Cud(u16),
    Cuf(u16),
    Cub(u16),
    Ind,
    Ri,
    Su(u16),
    Sd(u16),
    Il(u16),
    Dl(u16),
    Ich(u16),
    Dch(u16),
    Ech(u16),
    El(u8),
    Ed(u8),
    /// DECSTBM top;bottom, 1-based, top < bottom.
    Stbm(u16, u16),
    Awm(bool),
    Sgr(u8),
    /// SGR 38 direct colour, in the `;` form or ITU's `:` form (with the empty colour space).
    SgrRgb {
        r: u8,
        g: u8,
        b: u8,
        colon: bool,
    },
}

impl Op {
    pub(crate) fn bytes(&self, out: &mut StdVec<u8>) {
        use std::io::Write as _;
        let _ = match self {
            Op::Print(s) => write!(out, "{s}"),
            Op::Cr => write!(out, "\r"),
            Op::Lf => write!(out, "\n"),
            Op::Bs => write!(out, "\x08"),
            Op::Cup(r, c) => write!(out, "\x1b[{r};{c}H"),
            Op::Cuu(n) => write!(out, "\x1b[{n}A"),
            Op::Cud(n) => write!(out, "\x1b[{n}B"),
            Op::Cuf(n) => write!(out, "\x1b[{n}C"),
            Op::Cub(n) => write!(out, "\x1b[{n}D"),
            Op::Ind => write!(out, "\x1bD"),
            Op::Ri => write!(out, "\x1bM"),
            Op::Su(n) => write!(out, "\x1b[{n}S"),
            Op::Sd(n) => write!(out, "\x1b[{n}T"),
            Op::Il(n) => write!(out, "\x1b[{n}L"),
            Op::Dl(n) => write!(out, "\x1b[{n}M"),
            Op::Ich(n) => write!(out, "\x1b[{n}@"),
            Op::Dch(n) => write!(out, "\x1b[{n}P"),
            Op::Ech(n) => write!(out, "\x1b[{n}X"),
            Op::El(m) => write!(out, "\x1b[{m}K"),
            Op::Ed(m) => write!(out, "\x1b[{m}J"),
            Op::Stbm(t, b) => write!(out, "\x1b[{t};{b}r"),
            Op::Awm(on) => write!(out, "\x1b[?7{}", if *on { 'h' } else { 'l' }),
            Op::Sgr(p) => write!(out, "\x1b[{p}m"),
            Op::SgrRgb { r, g, b, colon: false } => write!(out, "\x1b[38;2;{r};{g};{b}m"),
            Op::SgrRgb { r, g, b, colon: true } => write!(out, "\x1b[38:2::{r}:{g}:{b}m"),
        };
    }

    /// Draws one operation for a `cols` × `rows` screen. Printing and moving the cursor back
    /// over what was printed come most often, so cells are overwritten and lines wrap.
    pub(crate) fn draw(tape: &Tape, cols: u16, rows: u16) -> Op {
        let n = |tape: &Tape, most: u16| tape.int_in(0..=most, "count");
        match tape.below(25, "op") {
            0..=5 => {
                let len = tape.int_in(1..=12usize, "print length");
                let s = (0..len)
                    .map(|_| *tape.pick(&['a', 'b', 'X', 'Y', ' ', '#'], "char").unwrap())
                    .collect();
                Op::Print(s)
            }
            6 => Op::Cr,
            7 => Op::Lf,
            8 => match tape.below(3, "small") {
                0 => Op::Bs,
                1 => Op::Ind,
                _ => Op::Ri,
            },
            9 | 10 => Op::Cup(tape.int_in(0..=rows + 1, "row"), tape.int_in(0..=cols + 1, "col")),
            11 => match tape.below(4, "move") {
                0 => Op::Cuu(n(tape, rows)),
                1 => Op::Cud(n(tape, rows)),
                2 => Op::Cuf(n(tape, cols)),
                _ => Op::Cub(n(tape, cols)),
            },
            12 => Op::Su(n(tape, rows + 1)),
            13 => Op::Sd(n(tape, rows + 1)),
            14 => Op::Il(n(tape, rows + 1)),
            15 => Op::Dl(n(tape, rows + 1)),
            16 => Op::Ich(n(tape, cols + 1)),
            17 => Op::Dch(n(tape, cols + 1)),
            18 => Op::Ech(n(tape, cols + 1)),
            19 => Op::El(tape.int_in(0..=2, "el")),
            20 => Op::Ed(tape.int_in(0..=3, "ed")),
            21 => {
                let top = tape.int_in(1..=rows - 1, "top");
                Op::Stbm(top, tape.int_in(top + 1..=rows, "bottom"))
            }
            22 => Op::Awm(tape.below(4, "awm") != 0),
            23 => Op::SgrRgb {
                r: tape.int_in(0..=255, "r"),
                g: tape.int_in(0..=255, "g"),
                b: tape.int_in(0..=255, "b"),
                colon: tape.below(2, "colon") != 0,
            },
            _ => Op::Sgr(*tape.pick(&[0u8, 1, 3, 7, 22, 23, 27, 31, 32, 34, 39], "sgr").unwrap()),
        }
    }
}

/// The model terminal.
pub(crate) struct Model {
    pub cols: usize,
    pub rows: usize,
    pub history: StdVec<MRow>,
    pub grid: StdVec<MRow>,
    pub x: usize,
    pub y: usize,
    pub pending_wrap: bool,
    pub style: MStyle,
    top: usize,
    bottom: usize,
    pub autowrap: bool,
}

impl Model {
    pub(crate) fn new(cols: usize, rows: usize) -> Model {
        Model {
            cols,
            rows,
            history: StdVec::new(),
            grid: (0..rows).map(|_| MRow::blank(cols)).collect(),
            x: 0,
            y: 0,
            pending_wrap: false,
            style: MStyle::default(),
            top: 0,
            bottom: rows - 1,
            autowrap: true,
        }
    }

    pub(crate) fn apply(&mut self, op: &Op) {
        let def1 = |n: u16| usize::from(n.max(1));
        match *op {
            Op::Print(ref s) => s.chars().for_each(|c| self.print(c)),
            Op::Cr => {
                self.x = 0;
                self.pending_wrap = false;
            }
            Op::Lf | Op::Ind => self.index(),
            Op::Bs => {
                self.x -= self.x.min(1);
                self.pending_wrap = false;
            }
            Op::Cup(r, c) => {
                self.y = def1(r).min(self.rows) - 1;
                self.x = def1(c).min(self.cols) - 1;
                self.pending_wrap = false;
            }
            Op::Cuu(n) => {
                let most = if self.y >= self.top { self.y - self.top } else { self.y };
                self.y -= most.min(def1(n));
                self.pending_wrap = false;
            }
            Op::Cud(n) => {
                let most = if self.y <= self.bottom {
                    self.bottom - self.y
                } else {
                    self.rows - self.y - 1
                };
                self.y += most.min(def1(n));
                self.pending_wrap = false;
            }
            Op::Cuf(n) => {
                self.x += (self.cols - 1 - self.x).min(def1(n));
                self.pending_wrap = false;
            }
            Op::Cub(n) => {
                self.x -= self.x.min(def1(n));
                self.pending_wrap = false;
            }
            Op::Ri => {
                // T:2082: at the region's top it is SD 1 (cursor and pending wrap kept).
                if self.y == self.top {
                    self.scroll_down(1);
                } else {
                    let most = if self.y >= self.top { self.y - self.top } else { self.y };
                    self.y -= most.min(1);
                    self.pending_wrap = false;
                }
            }
            Op::Su(n) => self.scroll_up(usize::from(n)),
            Op::Sd(n) => self.scroll_down(usize::from(n)),
            Op::Il(n) => self.insert_lines(usize::from(n)),
            Op::Dl(n) => self.delete_lines(usize::from(n)),
            Op::Ich(n) => self.insert_blanks(def1(n)),
            Op::Dch(n) => self.delete_chars(usize::from(n)),
            Op::Ech(n) => {
                let end = (self.x + def1(n)).min(self.cols);
                self.clear(self.y, self.x, end);
                self.reset_wrap();
            }
            Op::El(m) => self.erase_line(m),
            Op::Ed(m) => match m {
                0 => {
                    self.erase_line(0);
                    for y in self.y + 1..self.rows {
                        self.grid[y] = MRow::blank(self.cols);
                    }
                }
                1 => {
                    self.erase_line(1);
                    for y in 0..self.y {
                        self.grid[y] = MRow::blank(self.cols);
                    }
                }
                2 => {
                    for y in 0..self.rows {
                        self.grid[y] = MRow::blank(self.cols);
                    }
                    self.pending_wrap = false;
                }
                _ => self.history.clear(),
            },
            Op::Stbm(t, b) => {
                self.top = usize::from(t) - 1;
                self.bottom = usize::from(b) - 1;
                self.x = 0;
                self.y = 0;
                self.pending_wrap = false;
            }
            Op::Awm(on) => self.autowrap = on,
            Op::Sgr(p) => match p {
                0 => self.style = MStyle::default(),
                1 => self.style.bold = true,
                3 => self.style.italic = true,
                7 => self.style.inverse = true,
                22 => self.style.bold = false,
                23 => self.style.italic = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.fg = MFg::Palette(p - 30),
                _ => self.style.fg = MFg::None,
            },
            Op::SgrRgb { r, g, b, .. } => self.style.fg = MFg::Rgb(r, g, b),
        }
    }

    /// T:882, ASCII: wrap first if pending (printWrap: the row wraps, the next continues it,
    /// even when index left the cursor on the same row), write, then move or set pending.
    fn print(&mut self, c: char) {
        if self.pending_wrap && self.autowrap {
            self.grid[self.y].wrap = true;
            self.index();
            self.x = 0;
            self.grid[self.y].cont = true;
        }
        self.grid[self.y].cells[self.x] = MCell { ch: Some(c), style: self.style };
        if self.x == self.cols - 1 {
            self.pending_wrap = true;
        } else {
            self.x += 1;
        }
    }

    /// T:1969.
    fn index(&mut self) {
        self.pending_wrap = false;
        if self.y < self.top || self.y > self.bottom {
            if self.y < self.rows - 1 {
                self.y += 1;
            }
            return;
        }
        if self.y == self.bottom {
            if self.top == 0 {
                self.scroll_above(self.bottom);
            } else {
                self.region_up(self.top, self.bottom);
            }
            return;
        }
        self.y += 1;
    }

    /// Screen.zig cursorScrollAbove from row `at`: the top row goes into scrollback, rows up
    /// to `at` move up with their flags, and a new blank row takes `at`.
    fn scroll_above(&mut self, at: usize) {
        let top = self.grid.remove(0);
        self.history.push(top);
        self.grid.insert(at, MRow::blank(self.cols));
    }

    /// Screen.zig cursorScrollRegionUp: the top row's cells are cleared and it rotates to the
    /// bottom, keeping its wrap flag (clearCells leaves the row's flags: a quirk kept).
    fn region_up(&mut self, top: usize, bottom: usize) {
        for c in &mut self.grid[top].cells {
            *c = MCell::default();
        }
        self.grid[top..=bottom].rotate_left(1);
    }

    /// T:2207 SU: a region from the top pushes into scrollback; otherwise DL at the top.
    fn scroll_up(&mut self, n: usize) {
        let saved = (self.x, self.y, self.pending_wrap);
        if self.top == 0 {
            for _ in 0..n.min(self.bottom + 1) {
                self.scroll_above(self.bottom);
            }
        } else {
            self.x = 0;
            self.y = self.top;
            self.delete_lines(n);
        }
        (self.x, self.y, self.pending_wrap) = saved;
    }

    /// T:2185 SD: IL at the top, the cursor and its pending wrap kept.
    fn scroll_down(&mut self, n: usize) {
        let saved = (self.x, self.y, self.pending_wrap);
        self.x = 0;
        self.y = self.top;
        self.insert_lines(n);
        (self.x, self.y, self.pending_wrap) = saved;
    }

    /// T:2470: the rows swapped lose their wrap flags; rows only cleared keep theirs.
    fn insert_lines(&mut self, n: usize) {
        if n == 0 || self.y < self.top || self.y > self.bottom {
            return;
        }
        let rem = self.bottom - self.y + 1;
        let n = n.min(rem);
        for i in (0..rem).rev() {
            let cur = self.y + i;
            if i >= n {
                let off = cur - n;
                self.grid[cur].unwrap_flags();
                self.grid[off].unwrap_flags();
                self.grid.swap(cur, off);
            } else {
                for c in &mut self.grid[cur].cells {
                    *c = MCell::default();
                }
            }
        }
        self.x = 0;
        self.pending_wrap = false;
    }

    /// T:2677, as IL the other way.
    fn delete_lines(&mut self, n: usize) {
        if n == 0 || self.y < self.top || self.y > self.bottom {
            return;
        }
        let rem = self.bottom - self.y + 1;
        let n = n.min(rem);
        for i in 0..rem {
            let cur = self.y + i;
            if i < rem - n {
                let off = cur + n;
                self.grid[cur].unwrap_flags();
                self.grid[off].unwrap_flags();
                self.grid.swap(cur, off);
            } else {
                for c in &mut self.grid[cur].cells {
                    *c = MCell::default();
                }
            }
        }
        self.x = 0;
        self.pending_wrap = false;
    }

    /// T:2862: pending wrap cleared; cells shift right, the ones past the margin lost.
    fn insert_blanks(&mut self, n: usize) {
        self.pending_wrap = false;
        let row = &mut self.grid[self.y].cells;
        let rem = self.cols - self.x;
        let n = n.min(rem);
        row[self.x..].rotate_right(n);
        for c in &mut row[self.x..self.x + n] {
            *c = MCell::default();
        }
    }

    /// T:2963: count 0 is nothing; cells shift left; the row's soft wrap reset.
    fn delete_chars(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let row = &mut self.grid[self.y].cells;
        let rem = self.cols - self.x;
        let n = n.min(rem);
        row[self.x..].rotate_left(n);
        for c in &mut row[self.cols - n..] {
            *c = MCell::default();
        }
        self.reset_wrap();
    }

    /// T:3065: right resets the soft wrap; every mode clears pending wrap.
    fn erase_line(&mut self, mode: u8) {
        match mode {
            0 => {
                self.reset_wrap();
                self.clear(self.y, self.x, self.cols);
            }
            1 => self.clear(self.y, 0, self.x + 1),
            _ => self.clear(self.y, 0, self.cols),
        }
        self.pending_wrap = false;
    }

    /// Screen.zig cursorResetWrap: a wrapping row stops, and the row below stops continuing.
    fn reset_wrap(&mut self) {
        self.pending_wrap = false;
        if !self.grid[self.y].wrap {
            return;
        }
        self.grid[self.y].wrap = false;
        if self.y + 1 < self.rows {
            self.grid[self.y + 1].cont = false;
        }
    }

    fn clear(&mut self, y: usize, x0: usize, x1: usize) {
        for c in &mut self.grid[y].cells[x0..x1] {
            *c = MCell::default();
        }
    }

    /// Every row, scrollback first.
    pub(crate) fn all_rows(&self) -> impl Iterator<Item = &MRow> {
        self.history.iter().chain(self.grid.iter())
    }
}

/// A row as a reflow reads it.
pub(crate) struct LineRow {
    /// The row's cells as text, empty cells as `\0`, untrimmed.
    pub text: String,
    pub wrap: bool,
    pub cont: bool,
    /// The cursor's column, on the cursor's row.
    pub cursor: Option<usize>,
}

/// The lines a reflowing resize must keep, read as ghostty's `reflowRow` reads rows
/// (PageList.zig:1530): a soft-wrapped row is all its cells, another row up to its last
/// non-empty cell, or the cursor's column; a row with nothing to write is a blank line, or
/// nothing at all when it continues a wrap (the next row then joins the wrapped line: a quirk
/// kept). Lines trimmed of trailing spaces, trailing empty lines dropped.
pub(crate) fn logical_lines(rows: impl Iterator<Item = LineRow>) -> StdVec<String> {
    let mut out = String::new();
    let mut new_rows = 0usize;
    for row in rows {
        let chars: StdVec<char> = row.text.chars().collect();
        let mut len = if row.wrap {
            chars.len()
        } else {
            chars.iter().rposition(|&c| c != '\0').map_or(0, |i| i + 1)
        };
        if let Some(x) = row.cursor {
            len = len.max(x + 1);
        }
        if len == 0 {
            if !row.cont {
                new_rows += 1;
            }
            continue;
        }
        for _ in 0..new_rows {
            out.push('\n');
        }
        new_rows = 0;
        out.extend(&chars[..len.min(chars.len())]);
        if !row.wrap {
            new_rows += 1;
        }
    }
    let mut lines: StdVec<String> =
        out.split('\n').map(|l| l.replace('\0', " ").trim_end().into()).collect();
    while lines.last().is_some_and(|l: &String| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Rows each cut to `cols` and trimmed, trailing empty rows dropped: what a resize without
/// reflow must keep.
pub(crate) fn cut_rows<'r>(rows: impl Iterator<Item = String> + 'r, cols: usize) -> StdVec<String> {
    let mut out: StdVec<String> =
        rows.map(|t| t.chars().take(cols).collect::<String>().trim_end().into()).collect();
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}
