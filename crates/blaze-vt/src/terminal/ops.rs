//! What the control functions do to the screen: cursor motion, scrolling, erasing, insert and
//! delete, tabs, margins, screens, reset and resize (Terminal.zig, by line in each comment).

use blaze_own::planted::Planted;

use super::{
    Effect, Mode, MouseEvents, MouseFormat, Region, StatusDisplay, TAB_INTERVAL, Terminal, Which,
};
use crate::page::{Cell, RowPrompt, SemanticContent, Wide};
use crate::plant::VtPlant;
use crate::screen::{Charsets, ProtectedMode, Resize, SavedCursor, Screen, Slot};
use crate::style::Style;

/// What ED erases (csi.zig `EraseDisplay`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EraseDisplay {
    Below,
    Above,
    Complete,
    Scrollback,
    ScrollComplete,
}

/// What EL erases.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EraseLine {
    Right,
    Left,
    Complete,
}

impl<'a, P: Planted<VtPlant>> Terminal<'a, P> {
    // ---- cursor motion ---------------------------------------------------------------------

    /// CR (1475).
    pub(super) fn carriage_return(&mut self) {
        let left = self.region.left;
        let origin = self.modes.get(Mode::Origin);
        let s = self.scr();
        s.cursor.pending_wrap = false;
        s.cursor.x = if origin || s.cursor.x >= left { left } else { 0 };
    }

    /// LF, VT, FF (1489).
    pub(super) fn linefeed(&mut self) {
        self.index();
        if self.modes.get(Mode::Linefeed) {
            self.carriage_return();
        }
    }

    /// CUU (1502): to the top margin when below it, else the top of the screen.
    pub(super) fn cursor_up(&mut self, n: usize) {
        let top = self.region.top;
        let s = self.scr();
        s.cursor.pending_wrap = false;
        let max = if s.cursor.y >= top { s.cursor.y - top } else { s.cursor.y };
        s.cursor.y -= max.min(n.max(1));
    }

    /// CUD (1520).
    pub(super) fn cursor_down(&mut self, n: usize) {
        let bottom = self.region.bottom;
        let rows = self.rows;
        let s = self.scr();
        s.cursor.pending_wrap = false;
        let max = if s.cursor.y <= bottom { bottom - s.cursor.y } else { rows - s.cursor.y - 1 };
        s.cursor.y += max.min(n.max(1));
    }

    /// CUF (1537).
    pub(super) fn cursor_right(&mut self, n: usize) {
        let right = self.region.right;
        let cols = self.cols;
        let s = self.scr();
        s.cursor.pending_wrap = false;
        let max = if s.cursor.x <= right { right - s.cursor.x } else { cols - s.cursor.x - 1 };
        s.cursor.x += max.min(n.max(1));
    }

    /// CUB and BS (1551), with xterm's reverse wrap under modes 45 and 1045.
    pub(super) fn cursor_left(&mut self, n: usize) {
        #[derive(PartialEq)]
        enum Wrap {
            None,
            Reverse,
            Extended,
        }
        let wrap = if !self.modes.get(Mode::Wraparound) {
            Wrap::None
        } else if self.modes.get(Mode::ReverseWrapExtended) {
            Wrap::Extended
        } else if self.modes.get(Mode::ReverseWrap) {
            Wrap::Reverse
        } else {
            Wrap::None
        };
        let mut count = n.max(1);
        let region = self.region;
        let s = self.scr();
        if wrap == Wrap::None {
            s.cursor.x -= count.min(s.cursor.x);
            s.cursor.pending_wrap = false;
            return;
        }
        if s.cursor.pending_wrap {
            count -= 1;
            s.cursor.pending_wrap = false;
        }
        let (top, bottom, right) = (region.top, region.bottom, region.right);
        let left = if s.cursor.x < region.left { 0 } else { region.left };
        if s.cursor.x == left && wrap == Wrap::Reverse && s.cursor.y <= top {
            s.cursor.x = left;
            s.cursor.y = top;
            return;
        }
        loop {
            let max = s.cursor.x - left;
            let amount = max.min(count);
            count -= amount;
            s.cursor.x -= amount;
            if count == 0 {
                break;
            }
            if s.cursor.y == top {
                if wrap != Wrap::Extended {
                    break;
                }
                s.cursor.x = right;
                s.cursor.y = bottom;
                count -= 1;
                continue;
            }
            if s.cursor.y == 0 {
                break;
            }
            if wrap != Wrap::Extended {
                let up = s.r(s.cursor.y - 1);
                if !s.pages.row(up).wrap {
                    break;
                }
            }
            s.cursor.x = right;
            s.cursor.y -= 1;
            count -= 1;
        }
    }

    /// CUP (2100): 1-based, relative to the margins in origin mode, clamped.
    pub(super) fn set_cursor_pos(&mut self, row: usize, col: usize) {
        let (xo, yo, xmax, ymax) = if self.modes.get(Mode::Origin) {
            (self.region.left, self.region.top, self.region.right + 1, self.region.bottom + 1)
        } else {
            (0, 0, self.cols, self.rows)
        };
        let row = row.max(1);
        let col = col.max(1);
        let s = self.scr();
        s.cursor.pending_wrap = false;
        s.cursor.x = xmax.min(col.saturating_add(xo)).saturating_sub(1);
        s.cursor.y = ymax.min(row.saturating_add(yo)).saturating_sub(1);
    }

    /// The cursor's position as CUP takes it: 1-based, relative to the margins in origin mode.
    ///
    /// ghostty's stream handler passes the absolute position to `setCursorPos` for HPA, VPA,
    /// HPR and VPR (stream_terminal.zig:174-183), so in origin mode the margin offset is added
    /// twice. That is a bug, and blaze fixes it: the coordinate HPA leaves alone stays put.
    pub(super) fn cursor_pos_relative(&mut self) -> (usize, usize) {
        let origin = self.modes.get(Mode::Origin);
        let (top, left) = (self.region.top, self.region.left);
        let s = self.scr();
        if origin {
            (s.cursor.y.saturating_sub(top) + 1, s.cursor.x.saturating_sub(left) + 1)
        } else {
            (s.cursor.y + 1, s.cursor.x + 1)
        }
    }

    // ---- scrolling -------------------------------------------------------------------------

    /// IND (1969).
    pub(super) fn index(&mut self) {
        let region = self.region;
        let rows = self.rows;
        let cols = self.cols;
        self.scr().cursor.pending_wrap = false;
        let (y, x) = {
            let s = self.scr();
            (s.cursor.y, s.cursor.x)
        };
        if y < region.top || y > region.bottom {
            if y < rows - 1 {
                self.scr().cursor.y += 1;
            }
        } else if y == region.bottom && x >= region.left && x <= region.right {
            let full_width = region.left == 0 && region.right == cols - 1;
            let no_scrollback = self.scr().no_scrollback;
            if region.top == 0 && full_width && (!no_scrollback || region.bottom == 0) {
                self.scr().cursor_scroll_above();
            } else if !full_width {
                self.scroll_up(1);
            } else {
                self.scr().cursor_scroll_region_up(region.bottom - region.top);
            }
        } else if y < region.bottom {
            self.scr().cursor.y += 1;
        }
        let s = self.scr();
        if s.cursor.semantic != SemanticContent::Output {
            if s.cursor.semantic_clear_eol {
                s.cursor.semantic = SemanticContent::Output;
                s.cursor.semantic_clear_eol = false;
            } else {
                s.cursor_row_mut().prompt = RowPrompt::Continuation;
            }
        }
    }

    /// RI (2082).
    pub(super) fn reverse_index(&mut self) {
        let region = self.region;
        let s = self.scr();
        if s.cursor.y != region.top || s.cursor.x < region.left || s.cursor.x > region.right {
            self.cursor_up(1);
            return;
        }
        self.scroll_down(1);
    }

    /// SD (2185): IL at the top of the region, the cursor and its pending wrap kept.
    pub(super) fn scroll_down(&mut self, n: usize) {
        let region = self.region;
        let s = self.scr();
        let saved = (s.cursor.x, s.cursor.y, s.cursor.pending_wrap);
        s.cursor.x = region.left;
        s.cursor.y = region.top;
        self.insert_lines(n);
        let s = self.scr();
        (s.cursor.x, s.cursor.y, s.cursor.pending_wrap) = saved;
    }

    /// SU (2207): rows into scrollback when the region is the full width from the top, else
    /// DL at the top of the region; the cursor and its pending wrap kept.
    pub(super) fn scroll_up(&mut self, n: usize) {
        let region = self.region;
        let cols = self.cols;
        let rows = self.rows;
        let plant = self.plant.has(VtPlant::PendingWrapClearedOnScrollUp);
        let s = self.scr();
        let saved = (s.cursor.x, s.cursor.y, s.cursor.pending_wrap && !plant);
        if region.top == 0
            && region.left == 0
            && region.right == cols - 1
            && (!s.no_scrollback || region.bottom == rows - 1)
        {
            let count = n.min(region.bottom + 1);
            s.cursor.x = 0;
            s.cursor.y = region.bottom;
            for _ in 0..count {
                s.cursor_scroll_above();
            }
        } else {
            s.cursor.x = region.left;
            s.cursor.y = region.top;
            self.delete_lines(n);
        }
        let s = self.scr();
        (s.cursor.x, s.cursor.y, s.cursor.pending_wrap) = saved;
    }

    /// Clears what a row shift would split at the margins (ghostty's `rowWillBeShifted`, 2378).
    fn row_will_be_shifted(&mut self, r: usize) {
        let region = self.region;
        let cols = self.cols;
        let s = self.scr();
        if region.right == cols - 1 || region.left < 2 {
            let mut end = s.pages.cell(r, cols - 1);
            if end.wide() == Wide::SpacerHead {
                end.set_wide(Wide::Narrow);
                s.pages.set_cell(r, cols - 1, end);
            }
        }
        let lc = s.pages.cell(r, region.left);
        if lc.wide() == Wide::SpacerTail {
            s.unwide(r, region.left - 1);
        }
        let rc = s.pages.cell(r, region.right);
        if rc.wide() == Wide::Wide {
            s.unwide(r, region.right);
        }
    }

    /// Moves row `src` into `dst` within the margins (the whole row when the region is full
    /// width), `src` taking `dst`'s old cells.
    fn shift_row(&mut self, dst: usize, src: usize) {
        let region = self.region;
        let cols = self.cols;
        self.row_will_be_shifted(dst);
        self.row_will_be_shifted(src);
        let s = self.scr();
        if region.left == 0 && region.right == cols - 1 {
            for r in [dst, src] {
                let row = s.pages.row_mut(r);
                row.wrap = false;
                row.wrap_continuation = false;
            }
            s.pages.swap_rows(dst, src);
        } else {
            for x in region.left..=region.right {
                let a = s.pages.cell(dst, x);
                let b = s.pages.cell(src, x);
                s.pages.set_cell(dst, x, b);
                s.pages.set_cell(src, x, a);
            }
        }
        s.pages.row_mut(dst).dirty = true;
    }

    /// IL (2470).
    pub(super) fn insert_lines(&mut self, n: usize) {
        let region = self.region;
        let (y, x) = {
            let s = self.scr();
            (s.cursor.y, s.cursor.x)
        };
        if n == 0 || y < region.top || y > region.bottom || x < region.left || x > region.right {
            return;
        }
        let rem = region.bottom - y + 1;
        let count = n.min(rem);
        let base = self.scr().r(y);
        let mut i = rem;
        while i > 0 {
            let cur = base + i - 1;
            if i > count {
                self.shift_row(cur, cur - count);
            } else {
                self.row_will_be_shifted(cur);
                let s = self.scr();
                s.clear_cells(cur, region.left, region.right + 1);
                s.pages.row_mut(cur).dirty = true;
            }
            i -= 1;
        }
        let s = self.scr();
        s.cursor.x = region.left;
        s.cursor.y = y;
        s.cursor.pending_wrap = false;
    }

    /// DL (2677).
    pub(super) fn delete_lines(&mut self, n: usize) {
        let region = self.region;
        let (y, x) = {
            let s = self.scr();
            (s.cursor.y, s.cursor.x)
        };
        if n == 0 || y < region.top || y > region.bottom || x < region.left || x > region.right {
            return;
        }
        let rem = region.bottom - y + 1;
        let count = n.min(rem);
        let base = self.scr().r(y);
        for i in 0..rem {
            let cur = base + i;
            if i < rem - count {
                self.shift_row(cur, cur + count);
            } else {
                self.row_will_be_shifted(cur);
                let s = self.scr();
                s.clear_cells(cur, region.left, region.right + 1);
                s.pages.row_mut(cur).dirty = true;
            }
        }
        let s = self.scr();
        s.cursor.x = region.left;
        s.cursor.y = y;
        s.cursor.pending_wrap = false;
    }

    // ---- characters ------------------------------------------------------------------------

    /// ICH (2862).
    pub(super) fn insert_blanks(&mut self, n: usize) {
        let region = self.region;
        let s = self.scr();
        s.cursor.pending_wrap = false;
        let x = s.cursor.x;
        if n == 0 || x < region.left || x > region.right {
            return;
        }
        let r = s.cursor_r();
        if s.pages.cell(r, x).wide() == Wide::SpacerTail {
            s.clear_cells(r, x - 1, x + 1);
        }
        let rem = region.right - x + 1;
        if s.pages.cell(r, x + rem - 1).wide() == Wide::Wide {
            s.clear_cells(r, x + rem - 1, x + rem + 1);
        }
        let count = n.min(rem);
        let scroll = rem - count;
        if scroll > 0 {
            let end = x + scroll - 1;
            if s.pages.cell(r, end).wide() == Wide::Wide {
                s.clear_cells(r, end, end + 2);
            }
            let mut i = end + 1;
            while i > x {
                i -= 1;
                let a = s.pages.cell(r, i);
                let b = s.pages.cell(r, i + count);
                s.pages.set_cell(r, i, b);
                s.pages.set_cell(r, i + count, a);
            }
        }
        s.clear_cells(r, x, x + count);
        s.mark_cursor_dirty();
    }

    /// DCH (2963).
    pub(super) fn delete_chars(&mut self, n: usize) {
        let region = self.region;
        let s = self.scr();
        let x = s.cursor.x;
        if n == 0 || x < region.left || x > region.right {
            return;
        }
        let rem = region.right - x + 1;
        let count = n.min(rem);
        s.split_cell_boundary(x);
        s.split_cell_boundary(x + count);
        s.split_cell_boundary(region.right + 1);
        let r = s.cursor_r();
        let scroll = rem - count;
        for i in x..x + scroll {
            let a = s.pages.cell(r, i);
            let b = s.pages.cell(r, i + count);
            s.pages.set_cell(r, i, b);
            s.pages.set_cell(r, i + count, a);
        }
        s.clear_cells(r, x + scroll, x + rem);
        s.cursor_reset_wrap();
        s.mark_cursor_dirty();
    }

    /// ECH (3013).
    pub(super) fn erase_chars(&mut self, n: usize) {
        let cols = self.cols;
        let s = self.scr();
        let x = s.cursor.x;
        let remaining = cols - x;
        let mut count = remaining.min(n.max(1));
        let r = s.cursor_r();
        if count != remaining && s.pages.cell(r, x + count - 1).wide() == Wide::Wide {
            count += 1;
        }
        s.split_cell_boundary(x);
        s.split_cell_boundary(x + count);
        s.cursor_reset_wrap();
        s.mark_cursor_dirty();
        if s.protected_mode != ProtectedMode::Iso {
            s.clear_cells(r, x, x + count);
        } else {
            s.clear_unprotected_cells(r, x, x + count);
        }
    }

    /// EL (3065).
    pub(super) fn erase_line(&mut self, mode: EraseLine, protected_req: bool) {
        let cols = self.cols;
        let s = self.scr();
        let r = s.cursor_r();
        let (start, end) = match mode {
            EraseLine::Right => {
                let mut x = s.cursor.x;
                if x > 0 && s.cursor_cell().wide() == Wide::SpacerTail {
                    x -= 1;
                }
                s.cursor_reset_wrap();
                (x, cols)
            }
            EraseLine::Left => {
                let mut x = s.cursor.x;
                if s.cursor_cell().wide() == Wide::Wide {
                    x += 1;
                }
                (0, (x + 1).min(cols))
            }
            EraseLine::Complete => (0, cols),
        };
        s.cursor.pending_wrap = false;
        s.mark_cursor_dirty();
        if s.protected_mode == ProtectedMode::Iso || protected_req {
            s.clear_unprotected_cells(r, start, end);
        } else {
            s.clear_cells(r, start, end);
        }
    }

    /// ED (3145).
    pub(super) fn erase_display(&mut self, mode: EraseDisplay, protected_req: bool) {
        let rows = self.rows;
        let primary = self.active == Which::Primary;
        let s = self.scr();
        let protected = s.protected_mode == ProtectedMode::Iso || protected_req;
        match mode {
            EraseDisplay::ScrollComplete => {
                s.scroll_clear();
                s.cursor.pending_wrap = false;
                self.kitty_erase_display();
            }
            EraseDisplay::Complete => {
                // ^L at a prompt keeps the screen in scrollback (ghostty #905): the bottom row
                // decides, as ghostty's walk up the rows stops at the first.
                if primary {
                    let bottom = s.r(rows - 1);
                    if s.pages.row(bottom).prompt != RowPrompt::None {
                        s.scroll_clear();
                    }
                }
                s.clear_rows(0, rows - 1, protected);
                s.cursor.pending_wrap = false;
                s.all_dirty = true;
                self.kitty_erase_display();
            }
            EraseDisplay::Below => {
                self.erase_line(EraseLine::Right, protected_req);
                let s = self.scr();
                if s.cursor.y + 1 < rows {
                    let y = s.cursor.y + 1;
                    s.clear_rows(y, rows - 1, protected);
                }
            }
            EraseDisplay::Above => {
                self.erase_line(EraseLine::Left, protected_req);
                let s = self.scr();
                if s.cursor.y > 0 {
                    let y = s.cursor.y - 1;
                    s.clear_rows(0, y, protected);
                }
            }
            EraseDisplay::Scrollback => s.erase_history(),
        }
    }

    /// DECALN (3277).
    pub(super) fn decaln(&mut self) {
        let rows = self.rows;
        let cols = self.cols;
        let st = self.scr().cursor.style;
        self.scr().set_cursor_style(Style { fg: st.fg, bg: st.bg, ..Style::default() });
        self.region = Region { top: 0, bottom: rows - 1, left: 0, right: cols - 1 };
        self.modes.set(Mode::Origin, false);
        self.set_cursor_pos(1, 1);
        let s = self.scr();
        s.clear_rows(0, rows - 1, false);
        let id = s.cursor.style_id;
        for y in 0..rows {
            let r = s.r(y);
            for x in 0..cols {
                let old = s.pages.cell(r, x);
                s.styles.release(old.style_id(), 1);
                s.pages.set_cell(r, x, Cell::new(u32::from(b'E'), id, Wide::Narrow));
            }
            s.styles.add(id, cols as u32);
            s.pages.row_mut(r).dirty = true;
        }
        s.cursor.x = 0;
        s.cursor.y = 0;
    }

    // ---- tabs ------------------------------------------------------------------------------

    /// HT once (1910): to the next stop, at most to the right margin.
    pub(super) fn horizontal_tab(&mut self) {
        let right = self.region.right;
        loop {
            let x = self.scr().cursor.x;
            if x >= right {
                return;
            }
            self.scr().cursor.x = x + 1;
            if self.is_tabstop(x + 1) {
                return;
            }
        }
    }

    /// CBT once (1923).
    pub(super) fn horizontal_tab_back(&mut self) {
        let limit = if self.modes.get(Mode::Origin) { self.region.left } else { 0 };
        loop {
            let x = self.scr().cursor.x;
            if x <= limit {
                return;
            }
            self.scr().cursor.x = x - 1;
            if self.is_tabstop(x - 1) {
                return;
            }
        }
    }

    /// CHT / CBT `n` times, stopping when the cursor stops (stream_terminal.zig `horizontalTab`).
    pub(super) fn tabs(&mut self, n: usize, back: bool) {
        for _ in 0..n {
            let x = self.scr().cursor.x;
            if back {
                self.horizontal_tab_back();
            } else {
                self.horizontal_tab();
            }
            if self.scr().cursor.x == x {
                break;
            }
        }
    }

    /// HTS.
    pub(super) fn tab_set(&mut self) {
        let x = self.scr().cursor.x;
        self.tabstops[x] = true;
    }

    /// TBC 0: Tabstops.zig `unset` toggles the stop (`^=`), so clearing a column with no stop
    /// sets one: a quirk kept (Tabstops.zig:96).
    pub(super) fn tab_clear_current(&mut self) {
        let x = self.scr().cursor.x;
        self.tabstops[x] = !self.tabstops[x];
    }

    pub(super) fn tab_clear_all(&mut self) {
        self.tabstops.fill(false);
    }

    /// Stops every 8 columns (Tabstops.zig `reset`): 8, 16, …, not the last column.
    pub(super) fn tab_reset(&mut self) {
        let cols = self.tabstops.len();
        for (i, t) in self.tabstops.iter_mut().enumerate() {
            *t = i > 0 && i % TAB_INTERVAL == 0 && i < cols - 1;
        }
    }

    // ---- margins ---------------------------------------------------------------------------

    /// DECSTBM (2160).
    pub(super) fn set_top_bottom_margin(&mut self, top: usize, bottom: usize) {
        let top = top.max(1);
        let bottom = self.rows.min(if bottom == 0 { self.rows } else { bottom });
        if top >= bottom {
            return;
        }
        self.region.top = top - 1;
        self.region.bottom = bottom - 1;
        self.set_cursor_pos(1, 1);
    }

    /// DECSLRM (2171), only with mode 69.
    pub(super) fn set_left_right_margin(&mut self, left: usize, right: usize) {
        if !self.modes.get(Mode::EnableLeftRightMargin) {
            return;
        }
        let left = left.max(1);
        let right = self.cols.min(if right == 0 { self.cols } else { right });
        if left >= right {
            return;
        }
        self.region.left = left - 1;
        self.region.right = right - 1;
        self.set_cursor_pos(1, 1);
    }

    // ---- save and restore ------------------------------------------------------------------

    /// DECSC (1655).
    pub(super) fn save_cursor(&mut self) {
        let origin = self.modes.get(Mode::Origin);
        let s = self.scr();
        s.saved = Some(SavedCursor {
            x: s.cursor.x,
            y: s.cursor.y,
            style: s.cursor.style,
            protected: s.cursor.protected,
            pending_wrap: s.cursor.pending_wrap,
            origin,
            charset: s.charset,
        });
    }

    /// DECRC (1671): the saved cursor, or the home position with the default style.
    pub(super) fn restore_cursor(&mut self) {
        let cols = self.cols;
        let rows = self.rows;
        let s = self.scr();
        let saved = s.saved.unwrap_or(SavedCursor {
            x: 0,
            y: 0,
            style: Style::default(),
            protected: false,
            pending_wrap: false,
            origin: false,
            charset: Charsets::default(),
        });
        s.set_cursor_style(saved.style);
        s.charset = saved.charset;
        s.cursor.pending_wrap = saved.pending_wrap;
        s.cursor.protected = saved.protected;
        s.cursor.x = saved.x.min(cols - 1);
        s.cursor.y = saved.y.min(rows - 1);
        self.modes.set(Mode::Origin, saved.origin);
    }

    /// SPA, EPA, DECSCA (1709).
    pub(super) fn set_protected_mode(&mut self, mode: ProtectedMode) {
        let s = self.scr();
        match mode {
            ProtectedMode::Off => s.cursor.protected = false,
            m => {
                s.cursor.protected = true;
                s.protected_mode = m;
            }
        }
    }

    /// SS2/SS3 (single) or LS0–LS3 (locking) into GL, LS1R–LS3R into GR.
    pub(super) fn invoke_charset(&mut self, gr: bool, slot: Slot, single: bool) {
        let s = self.scr();
        if single {
            s.charset.single_shift = Some(slot);
        } else if gr {
            s.charset.gr = slot;
        } else {
            s.charset.gl = slot;
        }
    }

    // ---- screens ---------------------------------------------------------------------------

    /// Shows `to` (switchScreen, 3705); the screen left, if it changed.
    fn switch_screen(&mut self, to: Which) -> bool {
        if self.active == to {
            return false;
        }
        self.scr().end_hyperlink();
        let charset = self.scr().charset;
        if to == Which::Alternate && self.alternate.is_none() {
            match Screen::new(
                self.cols,
                self.rows,
                self.opts.page_rows.max(1),
                0,
                true,
                self.alloc,
                self.plant,
            ) {
                Ok(alt) => self.alternate = Some(alt),
                Err(f) => {
                    self.primary.note(crate::screen::Refused::Alloc(f));
                    return false;
                }
            }
        }
        self.active = to;
        let s = self.scr();
        s.charset = charset;
        s.all_dirty = true;
        s.pages.mark_all_dirty();
        true
    }

    /// The cursor of the screen not shown.
    fn other_cursor(&self) -> crate::screen::Cursor {
        match self.active {
            Which::Primary => self.alternate.as_ref().map_or(self.primary.cursor, |a| a.cursor),
            Which::Alternate => self.primary.cursor,
        }
    }

    /// Modes 47, 1047, 1049 (switchScreenMode).
    pub(super) fn switch_screen_mode(&mut self, mode: Mode, on: bool) {
        match mode {
            Mode::AltScreen if !on && self.active == Which::Alternate => {
                self.erase_display(EraseDisplay::Complete, false);
            }
            Mode::AltScreenSaveCursorClear if on => self.save_cursor(),
            _ => {}
        }
        let to = if on { Which::Alternate } else { Which::Primary };
        let changed = self.switch_screen(to);
        match mode {
            Mode::AltScreenSaveCursorClear => {
                if on {
                    self.erase_display(EraseDisplay::Complete, false);
                    if changed {
                        let c = self.other_cursor();
                        self.scr().copy_cursor(&c);
                    }
                } else {
                    self.restore_cursor();
                }
            }
            _ => {
                if changed {
                    let c = self.other_cursor();
                    self.scr().copy_cursor(&c);
                }
            }
        }
    }

    /// RIS (fullReset, 3885).
    pub(super) fn full_reset(&mut self) {
        // The alternate screen goes, its kitty images with it; the primary's are reset
        // (Screen.zig:396); the APC state and glyph glossary too.
        self.kitty_reset_screen(crate::kitty::exec::ScreenKey::Alternate);
        self.kitty_reset_screen(crate::kitty::exec::ScreenKey::Primary);
        self.apc.full_reset();
        self.active = Which::Primary;
        self.alternate = None;
        self.primary.reset();
        self.primary.cursor.shape = crate::screen::CursorShape::Block;
        self.modes.reset();
        self.flags = super::Flags::DEFAULT;
        self.tab_reset();
        self.previous_char = None;
        self.pwd.clear();
        self.title.clear();
        self.status_display = StatusDisplay::Main;
        self.region = Region { top: 0, bottom: self.rows - 1, left: 0, right: self.cols - 1 };
        self.primary.all_dirty = true;
    }

    /// Resizes (3537): the primary reflows with DECAWM, the alternate never; tab stops rebuilt
    /// when the columns change; the margins reset.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = usize::from(cols.max(1));
        let rows = usize::from(rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        if cols != self.cols {
            self.tabstops.clear();
            if self.tabstops.try_reserve(cols).is_err() {
                self.primary.note(crate::screen::Refused::Alloc(
                    blaze_own::AllocFailure::Refused {
                        what: "blaze_vt::terminal::Terminal::tabstops",
                        layout: core::alloc::Layout::new::<bool>(),
                    },
                ));
                return;
            }
            self.tabstops.resize(cols, false);
            self.tab_reset();
        }
        let reflow = self.modes.get(Mode::Wraparound);
        let prompt_redraw = self.flags.shell_redraws_prompt;
        self.primary.resize(Resize { cols, rows, reflow, prompt_redraw });
        if let Some(alt) = self.alternate.as_mut() {
            alt.resize(Resize {
                cols,
                rows,
                reflow: false,
                prompt_redraw: crate::screen::PromptRedraw::False,
            });
        }
        self.cols = cols;
        self.rows = rows;
        self.region = Region { top: 0, bottom: rows - 1, left: 0, right: cols - 1 };
        if self.modes.get(Mode::InBandSizeReports) {
            self.report_in_band_size();
        }
    }

    /// DECCOLM (3509): only with mode 40.
    fn deccolm(&mut self, on: bool) {
        if !self.modes.get(Mode::EnableMode3) {
            self.modes.set(Mode::Col132, false);
            return;
        }
        self.modes.set(Mode::Col132, on);
        let cols: u16 = if on { 132 } else { 80 };
        let rows = self.rows as u16;
        self.resize(cols, rows);
        self.effect(Effect::ColumnMode { cols });
        self.erase_display(EraseDisplay::Complete, false);
        self.set_cursor_pos(1, 1);
    }

    // ---- modes -----------------------------------------------------------------------------

    /// SM/RM/DECSET/DECRST and XTRESTORE (stream_terminal.zig `setMode`, with the stream
    /// handler's immediate reports for 1004, 2031 and 2048, SH:655).
    pub(super) fn set_mode(&mut self, mode: Mode, on: bool) {
        let was = self.modes.get(mode);
        self.modes.set(mode, on);
        match mode {
            Mode::Origin => self.set_cursor_pos(1, 1),
            Mode::EnableLeftRightMargin if !on => {
                self.region.left = 0;
                self.region.right = self.cols - 1;
            }
            Mode::AltScreenLegacy | Mode::AltScreen | Mode::AltScreenSaveCursorClear => {
                self.switch_screen_mode(mode, on);
            }
            Mode::SaveCursor => {
                if on {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            Mode::Col132 => self.deccolm(on),
            Mode::EnableMode3 if was != on => self.effect(Effect::EnableMode3(on)),
            Mode::ReverseColors if was != on => {
                self.scr().all_dirty = true;
                self.effect(Effect::ReverseColors(on));
            }
            Mode::SynchronizedOutput => self.effect(Effect::SynchronizedOutput(on)),
            Mode::FocusEvent if on => {
                use crate::input::focus::{self, FocusEvent};
                let ev = if self.focused { FocusEvent::Gained } else { FocusEvent::Lost };
                self.reply_bytes(&[focus::encode(ev, self.focus_opts())]);
            }
            Mode::InBandSizeReports if on => self.report_in_band_size(),
            Mode::ReportColorScheme if on => self.report_color_scheme(),
            Mode::MouseX10 => {
                self.flags.mouse_event = if on { MouseEvents::X10 } else { MouseEvents::None }
            }
            Mode::MouseNormal => {
                self.flags.mouse_event = if on { MouseEvents::Normal } else { MouseEvents::None }
            }
            Mode::MouseButton => {
                self.flags.mouse_event = if on { MouseEvents::Button } else { MouseEvents::None }
            }
            Mode::MouseAny => {
                self.flags.mouse_event = if on { MouseEvents::Any } else { MouseEvents::None }
            }
            Mode::MouseUtf8 => {
                self.flags.mouse_format = if on { MouseFormat::Utf8 } else { MouseFormat::X10 }
            }
            Mode::MouseSgr => {
                self.flags.mouse_format = if on { MouseFormat::Sgr } else { MouseFormat::X10 }
            }
            Mode::MouseUrxvt => {
                self.flags.mouse_format = if on { MouseFormat::Urxvt } else { MouseFormat::X10 }
            }
            Mode::MouseSgrPixels => {
                self.flags.mouse_format =
                    if on { MouseFormat::SgrPixels } else { MouseFormat::X10 };
            }
            _ => {}
        }
    }

    // ---- reports ---------------------------------------------------------------------------

    /// `CSI 48 ; rows ; cols ; hpx ; wpx t` (mode 2048), when the cell size is known.
    pub(super) fn report_in_band_size(&mut self) {
        self.size_report(crate::reply::SizeReport::Mode2048);
    }

    /// A size report, when the cell size is known (CSI 18 t needs none).
    pub(super) fn size_report(&mut self, style: crate::reply::SizeReport) {
        use crate::reply::SizeReport;
        let px = match (self.opts.cell_px, style) {
            (Some(px), _) => px,
            (None, SizeReport::Csi18) => super::Pixels::default(),
            (None, _) => return,
        };
        let size = crate::reply::Size {
            rows: self.rows as u16,
            columns: self.cols as u16,
            cell_width: px.width,
            cell_height: px.height,
        };
        self.reply(|o| crate::reply::size_report(o, style, size));
    }

    /// `CSI ? 997 ; 1|2 n`, when the host has said its scheme.
    pub(super) fn report_color_scheme(&mut self) {
        if let Some(scheme) = self.color_scheme {
            self.reply(|o| crate::reply::color_scheme(o, scheme));
        }
    }
}

impl<P: Planted<VtPlant>> Screen<'_, P> {
    /// A wide cell at `x` and its spacer tail become empty narrow cells (rowWillBeShifted's
    /// cleanup at a margin): the grapheme goes, the style stays.
    pub(crate) fn unwide(&mut self, r: usize, x: usize) {
        let mut w = self.pages.cell(r, x);
        if w.has_grapheme() {
            self.drop_graphemes(r, x);
            w = self.pages.cell(r, x);
        }
        w.set_codepoint(0);
        w.set_wide(Wide::Narrow);
        self.pages.set_cell(r, x, w);
        if x + 1 < self.cols() {
            let mut t = self.pages.cell(r, x + 1);
            t.set_wide(Wide::Narrow);
            self.pages.set_cell(r, x + 1, t);
        }
    }
}
