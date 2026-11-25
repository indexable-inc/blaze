//! Rows in fixed pages, as ghostty's page.zig and PageList.zig hold them: a cell is 8 bytes, a
//! row is a small header naming where its cells are in its page, and a page holds a fixed
//! number of rows. The list of pages holds the scrollback and the active area as one run of
//! rows: row 0 is the oldest kept, the active area is the last `rows` rows.
//!
//! - **No per-row allocation.** A page is two buffers (cells, row headers) made once; a row
//!   pushed past the last page takes a new page, or, when the scrollback is over its byte cap,
//!   the oldest page back (ghostty's `grow`, PageList.zig:3495).
//! - **O(1) addressing.** Row `r` is slot `start + r` of the run of slots that the pages lay
//!   end to end; `start` is where row 0 sits in the first page (history erased from the front
//!   leaves a partial first page).
//! - **Row moves are header swaps** within a page (ghostty swaps `Row` structs the same way);
//!   across pages the cells are swapped.
//!
//! What a cell names out of line (style, graphemes, hyperlink) is the screen's
//! ([`crate::screen`]); this module stores and moves cells and never interprets those ids.

use blaze_own::planted::Planted;
use blaze_own::{Alloc, AllocFailure, Vec, VecDeque};

use crate::plant::VtPlant;

/// How a cell takes part in a wide character.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wide {
    Narrow = 0,
    /// The first cell of a 2-wide character; its codepoint is here.
    Wide = 1,
    /// The second cell of a 2-wide character, empty.
    SpacerTail = 2,
    /// The last cell of a soft-wrapped row whose wide character did not fit and went to the
    /// next row.
    SpacerHead = 3,
}

/// What OSC 133 says a cell is (ghostty's `Cell.SemanticContent`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SemanticContent {
    #[default]
    Output = 0,
    Input = 1,
    Prompt = 2,
}

/// One cell, 8 bytes: codepoint (21 bits), style id (16), wide (2), protected, grapheme,
/// hyperlink, background-only, semantic content (2), and the id of its out-of-line entry (19
/// bits: graphemes and hyperlink, the screen's `ext` table) when it has one.
///
/// A background-only cell is what an erase leaves when the cursor's style has a background:
/// no text, and its style id names a style with only that background (ghostty's
/// `bg_color_palette` / `bg_color_rgb` cells). It is not empty for reflow and trimming.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell(u64);

const CP_BITS: u32 = 21;
const STYLE_SHIFT: u32 = 21;
const WIDE_SHIFT: u32 = 37;
const PROTECTED_BIT: u64 = 1 << 39;
const GRAPHEME_BIT: u64 = 1 << 40;
const HYPERLINK_BIT: u64 = 1 << 41;
const BG_BIT: u64 = 1 << 42;
const SEMANTIC_SHIFT: u32 = 43;
const EXT_SHIFT: u32 = 45;
/// The largest out-of-line entry id a cell can name.
pub const MAX_EXT: u32 = (1 << 19) - 1;

impl Cell {
    /// The empty cell: no text, default style, narrow.
    pub const BLANK: Cell = Cell(0);

    /// A narrow or wide cell holding `cp` in style `style`.
    pub const fn new(cp: u32, style: u16, wide: Wide) -> Cell {
        Cell(cp as u64 | (style as u64) << STYLE_SHIFT | (wide as u64) << WIDE_SHIFT)
    }

    /// A background-only cell: no text, style `style` (one with only a background).
    pub const fn bg(style: u16) -> Cell {
        Cell((style as u64) << STYLE_SHIFT | BG_BIT)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The codepoint, 0 for none (the first of a grapheme's codepoints).
    pub const fn codepoint(self) -> u32 {
        (self.0 & ((1 << CP_BITS) - 1)) as u32
    }

    pub fn set_codepoint(&mut self, cp: u32) {
        self.0 = (self.0 & !((1 << CP_BITS) - 1)) | u64::from(cp);
    }

    pub const fn style_id(self) -> u16 {
        (self.0 >> STYLE_SHIFT) as u16
    }

    pub fn set_style_id(&mut self, id: u16) {
        self.0 = (self.0 & !(0xFFFF << STYLE_SHIFT)) | u64::from(id) << STYLE_SHIFT;
    }

    pub const fn wide(self) -> Wide {
        match (self.0 >> WIDE_SHIFT) & 3 {
            0 => Wide::Narrow,
            1 => Wide::Wide,
            2 => Wide::SpacerTail,
            _ => Wide::SpacerHead,
        }
    }

    pub fn set_wide(&mut self, wide: Wide) {
        self.0 = (self.0 & !(3 << WIDE_SHIFT)) | (wide as u64) << WIDE_SHIFT;
    }

    pub const fn protected(self) -> bool {
        self.0 & PROTECTED_BIT != 0
    }

    pub fn set_protected(&mut self, on: bool) {
        self.set_bit(PROTECTED_BIT, on);
    }

    /// Whether more codepoints follow the first, in the cell's ext entry.
    pub const fn has_grapheme(self) -> bool {
        self.0 & GRAPHEME_BIT != 0
    }

    pub fn set_grapheme(&mut self, on: bool) {
        self.set_bit(GRAPHEME_BIT, on);
    }

    /// Whether the cell is in a hyperlink, named in its ext entry.
    pub const fn has_hyperlink(self) -> bool {
        self.0 & HYPERLINK_BIT != 0
    }

    pub fn set_hyperlink(&mut self, on: bool) {
        self.set_bit(HYPERLINK_BIT, on);
    }

    pub const fn is_bg(self) -> bool {
        self.0 & BG_BIT != 0
    }

    pub const fn semantic(self) -> SemanticContent {
        match (self.0 >> SEMANTIC_SHIFT) & 3 {
            1 => SemanticContent::Input,
            2 => SemanticContent::Prompt,
            _ => SemanticContent::Output,
        }
    }

    pub fn set_semantic(&mut self, s: SemanticContent) {
        self.0 = (self.0 & !(3 << SEMANTIC_SHIFT)) | (s as u64) << SEMANTIC_SHIFT;
    }

    /// The id of the cell's out-of-line entry, 0 for none.
    pub const fn ext(self) -> u32 {
        (self.0 >> EXT_SHIFT) as u32
    }

    pub fn set_ext(&mut self, id: u32) {
        debug_assert!(id <= MAX_EXT);
        self.0 = (self.0 & !(u64::from(MAX_EXT) << EXT_SHIFT)) | u64::from(id) << EXT_SHIFT;
    }

    /// Whether the cell shows text (ghostty's `hasText`).
    pub const fn has_text(self) -> bool {
        self.codepoint() != 0
    }

    /// No text, narrow, not a background cell (ghostty's `isEmpty`): what reflow trims.
    pub const fn is_empty(self) -> bool {
        !self.has_text() && matches!(self.wide(), Wide::Narrow) && !self.is_bg()
    }

    fn set_bit(&mut self, bit: u64, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

impl core::fmt::Debug for Cell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Cell({:#x} s{} {:?}", self.codepoint(), self.style_id(), self.wide())?;
        if self.protected() {
            f.write_str(" prot")?;
        }
        if self.is_bg() {
            f.write_str(" bg")?;
        }
        if self.ext() != 0 {
            write!(f, " ext{}", self.ext())?;
        }
        f.write_str(")")
    }
}

/// OSC 133's mark on a row (ghostty's `Row.semantic_prompt`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RowPrompt {
    #[default]
    None,
    Prompt,
    Continuation,
}

/// A row's header: where its cells are in its page, and its flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    /// The first of the row's cells in its page's cell buffer.
    cells: u32,
    /// The row soft-wraps into the next.
    pub wrap: bool,
    /// The row is the continuation of a soft wrap.
    pub wrap_continuation: bool,
    /// Changed since damage was last taken.
    pub dirty: bool,
    pub prompt: RowPrompt,
    /// Holds a kitty unicode placeholder (U+10EEEE).
    pub kitty_placeholder: bool,
}

impl Row {
    const fn at(cells: u32) -> Row {
        Row {
            cells,
            wrap: false,
            wrap_continuation: false,
            dirty: true,
            prompt: RowPrompt::None,
            kitty_placeholder: false,
        }
    }

    /// The row's flags reset (ghostty's `row.* = .{ .cells = cells }`), marked dirty.
    pub fn reset(&mut self) {
        *self = Row::at(self.cells);
    }
}

/// A page: `page_rows` rows of `cols` cells each.
struct Page<'a> {
    cells: Vec<Cell, Alloc<'a>>,
    rows: Vec<Row, Alloc<'a>>,
}

impl<'a> Page<'a> {
    fn new(page_rows: usize, cols: usize, alloc: Alloc<'a>) -> Result<Page<'a>, AllocFailure> {
        let mut cells = Vec::try_with_capacity_in(page_rows * cols, alloc)?;
        cells.resize(page_rows * cols, Cell::BLANK);
        let mut rows = Vec::try_with_capacity_in(page_rows, alloc)?;
        for i in 0..page_rows {
            rows.push(Row::at((i * cols) as u32));
        }
        Ok(Page { cells, rows })
    }
}

/// What pushing a row did to the rows before it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grown {
    /// Rows taken from the front with the oldest page (scrollback pruned past its cap): every
    /// row index held before is that much less now.
    pub pruned: usize,
}

/// The run of rows, scrollback then active area, in pages.
pub struct PageList<'a, P> {
    alloc: Alloc<'a>,
    pages: VecDeque<Page<'a>, Alloc<'a>>,
    page_rows: usize,
    cols: usize,
    rows: usize,
    /// Where row 0 sits in the first page.
    start: usize,
    /// Rows held, scrollback and active area; at least `rows`.
    total: usize,
    /// The scrollback cap the caller asked for, in bytes of pages.
    max_bytes: usize,
    plant: P,
}

impl<'a, P: Planted<VtPlant>> PageList<'a, P> {
    /// `rows` blank rows of `cols` cells, scrollback capped at `max_bytes` of pages.
    pub fn new(
        cols: usize,
        rows: usize,
        page_rows: usize,
        max_bytes: usize,
        alloc: Alloc<'a>,
        plant: P,
    ) -> Result<PageList<'a, P>, AllocFailure> {
        assert!(
            cols > 0 && rows > 0 && page_rows > 0,
            "a page list needs a row, a column and a page row"
        );
        let mut list = PageList {
            alloc,
            pages: VecDeque::new_in(alloc),
            page_rows,
            cols,
            rows,
            start: 0,
            total: 0,
            max_bytes,
            plant,
        };
        for _ in 0..rows {
            list.grow()?;
        }
        Ok(list)
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn page_rows(&self) -> usize {
        self.page_rows
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// The index of the active area's first row.
    pub fn active_top(&self) -> usize {
        self.total - self.rows
    }

    /// Rows above the active area.
    pub fn scrollback(&self) -> usize {
        self.total - self.rows
    }

    /// Bytes one page takes.
    pub fn page_bytes(&self) -> usize {
        self.page_rows * (self.cols * size_of::<Cell>() + size_of::<Row>())
    }

    /// Bytes the pages take now.
    pub fn bytes(&self) -> usize {
        self.pages.len() * self.page_bytes()
    }

    /// The byte cap in force: the caller's, or enough for the active area and one page more
    /// (ghostty's `maxSize`, `minMaxSize`, PageList.zig:491).
    fn max_size(&self) -> usize {
        let pages = self.rows.div_ceil(self.page_rows) + 1;
        self.max_bytes.max(pages * self.page_bytes())
    }

    fn locate(&self, r: usize) -> (usize, usize) {
        debug_assert!(r < self.total, "row {r} of {}", self.total);
        let slot = self.start + r;
        (slot / self.page_rows, slot % self.page_rows)
    }

    pub fn row(&self, r: usize) -> &Row {
        let (p, i) = self.locate(r);
        &self.pages[p].rows[i]
    }

    pub fn row_mut(&mut self, r: usize) -> &mut Row {
        let (p, i) = self.locate(r);
        &mut self.pages[p].rows[i]
    }

    pub fn cells(&self, r: usize) -> &[Cell] {
        let (p, i) = self.locate(r);
        let page = &self.pages[p];
        let at = page.rows[i].cells as usize;
        &page.cells[at..at + self.cols]
    }

    pub fn cells_mut(&mut self, r: usize) -> &mut [Cell] {
        let (p, i) = self.locate(r);
        let cols = self.cols;
        let page = &mut self.pages[p];
        let at = page.rows[i].cells as usize;
        &mut page.cells[at..at + cols]
    }

    /// The row's header and cells, both writable.
    pub fn row_and_cells_mut(&mut self, r: usize) -> (&mut Row, &mut [Cell]) {
        let (p, i) = self.locate(r);
        let cols = self.cols;
        let page = &mut self.pages[p];
        let at = page.rows[i].cells as usize;
        (&mut page.rows[i], &mut page.cells[at..at + cols])
    }

    pub fn cell(&self, r: usize, x: usize) -> Cell {
        self.cells(r)[x]
    }

    pub fn set_cell(&mut self, r: usize, x: usize, c: Cell) {
        self.cells_mut(r)[x] = c;
    }

    /// Marks every row dirty.
    pub fn mark_all_dirty(&mut self) {
        for p in self.pages.iter_mut() {
            for row in p.rows.iter_mut() {
                row.dirty = true;
            }
        }
    }

    /// Swaps rows `a` and `b`, header (flags) and cells.
    pub fn swap_rows(&mut self, a: usize, b: usize) {
        if a == b {
            return;
        }
        let (pa, ia) = self.locate(a);
        let (pb, ib) = self.locate(b);
        // The plant: a row just past a page boundary taken for one in the page before it.
        let same = if self.plant.has(VtPlant::PageBoundaryOffByOne) && ib == 0 && pb == pa + 1 {
            true
        } else {
            pa == pb
        };
        if same {
            self.pages[pa].rows.swap(ia, ib);
            return;
        }
        let (ra, rb) = (*self.row(a), *self.row(b));
        for x in 0..self.cols {
            let ca = self.cell(a, x);
            let cb = self.cell(b, x);
            self.set_cell(a, x, cb);
            self.set_cell(b, x, ca);
        }
        let ca = self.row(a).cells;
        let cb = self.row(b).cells;
        *self.row_mut(a) = Row { cells: ca, ..rb };
        *self.row_mut(b) = Row { cells: cb, ..ra };
    }

    /// Moves row `to` up to `from` and every row in `from..to` down by one (ghostty's
    /// `rotateOnceR` over `[from, to]`).
    pub fn rotate_down(&mut self, from: usize, to: usize) {
        let mut r = to;
        while r > from {
            self.swap_rows(r, r - 1);
            r -= 1;
        }
    }

    /// Moves row `from` down to `to` and every row in `from+1..=to` up by one (ghostty's
    /// `rotateOnce` over `[from, to]`).
    pub fn rotate_up(&mut self, from: usize, to: usize) {
        for r in from..to {
            self.swap_rows(r, r + 1);
        }
    }

    /// Pushes a blank row at the end (ghostty's `grow`): into a free slot of the last page,
    /// else a new page, or the oldest page back when the pages are at the byte cap and enough
    /// rows remain for the active area. The row comes blank, flags reset, dirty.
    /// How many rows from the front the next [`grow`](Self::grow) prunes (0 for none): the
    /// caller releases what their cells name first.
    pub fn grow_would_prune(&self) -> usize {
        let end = self.start + self.total;
        if end != self.pages.len() * self.page_rows {
            return 0;
        }
        let first_rows = self.page_rows - self.start;
        if self.pages.len() >= 2
            && self.bytes() + self.page_bytes() > self.max_size()
            && self.total - first_rows + 1 >= self.rows
        {
            first_rows
        } else {
            0
        }
    }

    pub fn grow(&mut self) -> Result<Grown, AllocFailure> {
        let mut pruned = 0;
        let end = self.start + self.total;
        if end == self.pages.len() * self.page_rows {
            let first_rows = self.page_rows - self.start;
            let recycled = if self.grow_would_prune() > 0 { self.pages.pop_front() } else { None };
            match recycled {
                Some(page) => {
                    self.total -= first_rows;
                    self.start = 0;
                    pruned = first_rows;
                    self.pages.push_back(page);
                }
                None => {
                    let page = Page::new(self.page_rows, self.cols, self.alloc)?;
                    if let Err(refused) = self.pages.try_push_back(page) {
                        drop(refused);
                        return Err(AllocFailure::Refused {
                            what: "blaze_vt::page::PageList::pages",
                            layout: core::alloc::Layout::new::<Page<'_>>(),
                        });
                    }
                }
            }
        }
        self.total += 1;
        let r = self.total - 1;
        let (row, cells) = self.row_and_cells_mut(r);
        row.reset();
        cells.fill(Cell::BLANK);
        Ok(Grown { pruned })
    }

    /// Drops the `n` oldest rows (`n` at most the scrollback). The caller has released what
    /// their cells named.
    pub fn erase_front(&mut self, n: usize) {
        assert!(n <= self.scrollback(), "erasing {n} rows of {} scrollback", self.scrollback());
        self.start += n;
        self.total -= n;
        while self.start >= self.page_rows {
            self.pages.pop_front();
            self.start -= self.page_rows;
        }
    }

    /// Drops the `n` newest rows; the caller has released what their cells named. Whole pages
    /// left empty at the end are freed.
    pub fn truncate_back(&mut self, n: usize) {
        assert!(n <= self.total);
        self.total -= n;
        let need = (self.start + self.total).div_ceil(self.page_rows).max(1);
        while self.pages.len() > need {
            self.pages.pop_back();
        }
    }

    /// Sets the active row count without moving rows: fewer rows push the top of the active
    /// area into scrollback; more need `total` rows to exist already.
    pub fn set_rows(&mut self, rows: usize) {
        assert!(rows > 0 && rows <= self.total);
        self.rows = rows;
    }

    /// Sets the active row count before growing to it (more rows than are held).
    pub fn set_rows_unchecked(&mut self, rows: usize) {
        assert!(rows > 0);
        self.rows = rows;
    }

    /// The scrollback cap.
    pub fn set_max_bytes(&mut self, max_bytes: usize) {
        self.max_bytes = max_bytes;
    }

    /// Every row dropped and the list as new: `rows` blank rows.
    pub fn reset(&mut self) -> Result<(), AllocFailure> {
        self.pages.clear();
        self.start = 0;
        self.total = 0;
        for _ in 0..self.rows {
            self.grow()?;
        }
        Ok(())
    }

    /// A new empty list of the same pages, caps and plant but `cols` columns, to rebuild into
    /// (resize). It holds no rows until grown.
    pub fn empty_like(&self, cols: usize, rows: usize) -> PageList<'a, P> {
        PageList {
            alloc: self.alloc,
            pages: VecDeque::new_in(self.alloc),
            page_rows: self.page_rows,
            cols,
            rows,
            start: 0,
            total: 0,
            max_bytes: self.max_bytes,
            plant: self.plant,
        }
    }

    /// Grows by one row without letting the cap prune: a rebuild pushes every row first and
    /// prunes after.
    pub fn grow_unpruned(&mut self) -> Result<(), AllocFailure> {
        let end = self.start + self.total;
        if end == self.pages.len() * self.page_rows {
            let page = Page::new(self.page_rows, self.cols, self.alloc)?;
            if let Err(refused) = self.pages.try_push_back(page) {
                drop(refused);
                return Err(AllocFailure::Refused {
                    what: "blaze_vt::page::PageList::pages",
                    layout: core::alloc::Layout::new::<Page<'_>>(),
                });
            }
        }
        self.total += 1;
        let r = self.total - 1;
        let (row, cells) = self.row_and_cells_mut(r);
        row.reset();
        cells.fill(Cell::BLANK);
        Ok(())
    }

    /// How many whole front pages a cap would prune now (after a rebuild): the pages beyond
    /// the cap whose rows are all scrollback, oldest first, keeping enough for the active area.
    pub fn prunable_front_rows(&self) -> usize {
        let pb = self.page_bytes();
        let mut pages = self.pages.len();
        let mut start = self.start;
        let mut total = self.total;
        let mut rows = 0;
        while pages >= 2 && pages * pb > self.max_size() {
            let first = self.page_rows - start;
            if total - first < self.rows {
                break;
            }
            total -= first;
            rows += first;
            start = 0;
            pages -= 1;
        }
        rows
    }
}
