//! Printing (Terminal.zig `print`, 882; `printWrap`, 1410; `printRepeat`, 361).

use blaze_own::planted::Planted;

use super::{Mode, StatusDisplay, Terminal};
use crate::page::Wide;
use crate::plant::VtPlant;

/// What a codepoint joined to a grapheme does to its width (ghostty's `graphemeWidthEffect`).
enum WidthEffect {
    Ignore,
    Wide,
    Narrow,
    NoChange,
}

fn width_effect(prev: u32, cp: u32) -> WidthEffect {
    if cp == 0xFE0F || cp == 0xFE0E {
        let base = char::from_u32(prev).is_some_and(blaze_unicode::is_emoji);
        if !base {
            return WidthEffect::Ignore;
        }
        return if cp == 0xFE0F { WidthEffect::Wide } else { WidthEffect::Narrow };
    }
    match char::from_u32(cp).and_then(blaze_unicode::char_width) {
        Some(0) => WidthEffect::NoChange,
        _ => WidthEffect::Wide,
    }
}

/// The width a codepoint takes alone: 1 up to 0xFF, else the table's, at most 2.
pub(crate) fn cp_width(c: u32) -> usize {
    if c <= 0xFF {
        return 1;
    }
    match char::from_u32(c).and_then(blaze_unicode::char_width) {
        Some(w) => w.min(2),
        None => 1,
    }
}

/// Whether `c` continues the grapheme `cps` (no break between them). Clusters are UAX #29's
/// legacy clusters (blaze-unicode); a grapheme past 32 codepoints takes no more.
fn continues(cps: &[u32], c: u32) -> bool {
    let mut buf = [0u8; 4 * 33];
    let mut n = 0;
    if cps.len() >= 32 {
        return false;
    }
    for &cp in cps.iter().chain(core::iter::once(&c)) {
        let Some(ch) = char::from_u32(cp) else { return false };
        n += ch.encode_utf8(&mut buf[n..]).len();
    }
    let Ok(s) = core::str::from_utf8(&buf[..n]) else { return false };
    blaze_unicode::graphemes(s).next().is_some_and(|first| first.len() == s.len())
}

impl<P: Planted<VtPlant>> Terminal<'_, P> {
    /// Prints `c` at the cursor.
    pub(super) fn print(&mut self, c: u32) {
        if self.status_display != StatusDisplay::Main {
            return;
        }
        let cols = self.cols;
        let region = self.region;
        let wrap = self.modes.get(Mode::Wraparound);
        let x = self.scr().cursor.x;
        let right_limit = if x > region.right { cols } else { region.right + 1 };

        if c > 255
            && self.modes.get(Mode::GraphemeCluster)
            && x > 0
            && self.print_grapheme(c, right_limit)
        {
            return;
        }

        let width = cp_width(c);
        if width == 0 {
            if self.modes.get(Mode::GraphemeCluster) {
                return;
            }
            let s = self.scr();
            let left = if wrap && s.cursor.pending_wrap { 0 } else { 1 };
            if s.cursor.x == 0 && left == 1 {
                return;
            }
            let r = s.cursor_r();
            let mut px = s.cursor.x - left;
            if s.pages.cell(r, px).wide() == Wide::SpacerTail {
                px -= 1;
            }
            let prev = s.pages.cell(r, px);
            if !prev.has_text() {
                return;
            }
            if (c == 0xFE0F || c == 0xFE0E)
                && !char::from_u32(prev.codepoint()).is_some_and(blaze_unicode::is_emoji)
            {
                return;
            }
            s.append_grapheme(r, px, c);
            return;
        }

        self.previous_char = Some(c);
        if self.scr().cursor.pending_wrap && wrap {
            self.print_wrap();
        }
        if self.modes.get(Mode::Insert) && self.scr().cursor.x + width < cols {
            self.insert_blanks(width);
        }
        if width == 1 {
            let s = self.scr();
            s.mark_cursor_dirty();
            s.print_cell(c, Wide::Narrow);
        } else if right_limit - region.left > 1 {
            if self.scr().cursor.x == right_limit - 1 {
                if !wrap {
                    return;
                }
                if right_limit == cols {
                    let s = self.scr();
                    s.cursor_row_mut().wrap = true;
                    s.print_cell(0, Wide::SpacerHead);
                } else {
                    self.scr().print_cell(0, Wide::Narrow);
                }
                self.print_wrap();
            }
            let s = self.scr();
            s.mark_cursor_dirty();
            s.print_cell(c, Wide::Wide);
            s.cursor.x += 1;
            s.print_cell(0, Wide::SpacerTail);
        } else {
            let s = self.scr();
            s.mark_cursor_dirty();
            s.print_cell(0, Wide::Narrow);
        }
        let s = self.scr();
        if s.cursor.x == right_limit - 1 {
            s.cursor.pending_wrap = true;
        } else {
            s.cursor.x += 1;
        }
    }

    /// Mode 2027: joins `c` to the grapheme left of the cursor if no break comes between them.
    /// Whether it was joined (or dropped as an ignored variation selector).
    fn print_grapheme(&mut self, c: u32, right_limit: usize) -> bool {
        let wrap = self.modes.get(Mode::Wraparound);
        let cols = self.cols;
        let s = self.scr();
        let x = s.cursor.x;
        let left = if wrap {
            usize::from(!s.cursor.pending_wrap)
        } else if x != right_limit - 1 {
            1
        } else {
            usize::from(s.cursor_cell().codepoint() == 0)
        };
        let r = s.cursor_r();
        let mut pl = left;
        if s.pages.cell(r, x - left).wide() == Wide::SpacerTail {
            pl = left + 1;
        }
        let mut px = x - pl;
        let mut pr = r;
        let prev = s.pages.cell(r, px);
        if prev.codepoint() == 0 {
            return false;
        }
        let mut cps = [0u32; 33];
        let g = s.graphemes(prev);
        if g.len() >= 32 {
            return false;
        }
        cps[0] = prev.codepoint();
        cps[1..=g.len()].copy_from_slice(g);
        let n = g.len() + 1;
        if !continues(&cps[..n], c) {
            return false;
        }
        match width_effect(cps[n - 1], c) {
            WidthEffect::Ignore => return true,
            WidthEffect::Wide if prev.wide() != Wide::Wide => {
                s.cursor.x -= pl;
                if s.cursor.x == right_limit - 1 {
                    if !wrap {
                        return true;
                    }
                    let row_wrap = right_limit == cols;
                    if row_wrap {
                        s.cursor_row_mut().wrap = true;
                    }
                    let prev_cp = prev.codepoint();
                    if prev.has_grapheme() {
                        let mut head = prev;
                        head.set_wide(if row_wrap { Wide::SpacerHead } else { Wide::Narrow });
                        head.set_codepoint(0);
                        s.pages.set_cell(r, px, head);
                        self.print_wrap();
                        let s = self.scr();
                        s.print_cell(prev_cp, Wide::Wide);
                        let nr = s.cursor_r();
                        if nr > 0 {
                            s.move_graphemes(nr - 1, right_limit - 1, nr, s.cursor.x);
                        }
                    } else {
                        s.print_cell(0, if row_wrap { Wide::SpacerHead } else { Wide::Narrow });
                        self.print_wrap();
                        self.scr().print_cell(prev_cp, Wide::Wide);
                    }
                    let s = self.scr();
                    pr = s.cursor_r();
                    px = s.cursor.x;
                } else {
                    let mut w = prev;
                    w.set_wide(Wide::Wide);
                    s.pages.set_cell(r, px, w);
                }
                let s = self.scr();
                s.cursor.x += 1;
                s.print_cell(0, Wide::SpacerTail);
                if s.cursor.x == right_limit - 1 {
                    s.cursor.pending_wrap = true;
                } else {
                    s.cursor.x += 1;
                }
            }
            WidthEffect::Narrow if prev.wide() == Wide::Wide => {
                let mut nw = prev;
                nw.set_wide(Wide::Narrow);
                s.pages.set_cell(r, px, nw);
                let tx = x - (pl - 1);
                let mut tail = s.pages.cell(r, tx);
                tail.set_wide(Wide::Narrow);
                s.pages.set_cell(r, tx, tail);
                if s.cursor.x == right_limit - 1 {
                    s.cursor.pending_wrap = false;
                } else {
                    s.cursor.x -= 1;
                }
            }
            _ => {}
        }
        let s = self.scr();
        s.mark_cursor_dirty();
        s.append_grapheme(pr, px, c);
        true
    }

    /// Soft-wraps to the next row (Terminal.zig:1410).
    pub(super) fn print_wrap(&mut self) {
        let cols = self.cols;
        let left = self.region.left;
        let s = self.scr();
        let mark_wrap = s.cursor.x == cols - 1;
        if mark_wrap {
            s.cursor_row_mut().wrap = true;
        }
        let old_semantic = s.cursor.semantic;
        let old_clear = s.cursor.semantic_clear_eol;
        self.index();
        let s = self.scr();
        s.cursor.x = left;
        s.cursor.pending_wrap = false;
        s.cursor.semantic = old_semantic;
        s.cursor.semantic_clear_eol = old_clear;
        if old_semantic == crate::page::SemanticContent::Prompt {
            s.cursor_row_mut().prompt = crate::page::RowPrompt::Continuation;
        }
        if mark_wrap {
            s.cursor_row_mut().wrap_continuation = true;
        }
    }

    /// REP: the last printed character `n` more times (at least once), if one was printed.
    pub(super) fn print_repeat(&mut self, n: usize) {
        if let Some(c) = self.previous_char {
            for _ in 0..n.max(1) {
                self.print(c);
            }
        }
    }
}
