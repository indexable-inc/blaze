//! Native `Cmd+F` Find-in-Scrollback & Output engine for `blaze-shell`.
//!
//! Operates directly over extracted [`crate::view::VisualLine`] rows so match highlights
//! align pixel-accurately with rendered text across command blocks, tables, previews, and PTYs.

use crate::view::{CellMetrics, VisualLine};

const COMFORT_MARGIN: f32 = 24.0;
const TARGET_VIEWPORT_FRACTION: f32 = 0.38;

/// Interactive state for the `Cmd+F` Find bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindBar {
    pub query: String,
    /// Byte offset into `query`, maintained on a valid UTF-8 char boundary.
    pub cursor: usize,
    pub case_sensitive: bool,
    /// 0-based index of the active match within `0..total_matches`.
    pub current: usize,
    pub total_matches: usize,
}

impl FindBar {
    /// Create a new find bar, optionally pre-populated with `initial` (filtered of `\r`/`\n` control chars).
    pub fn new(initial: Option<&str>) -> Self {
        let mut bar = Self {
            query: String::new(),
            cursor: 0,
            case_sensitive: false,
            current: 0,
            total_matches: 0,
        };
        if let Some(s) = initial {
            bar.insert_str(s);
        }
        bar
    }

    /// Insert text at the cursor, filtering out `\r`, `\n`, and other control characters.
    pub fn insert_str(&mut self, s: &str) {
        self.cursor = clamp_char_boundary(&self.query, self.cursor);
        let filtered: String = s
            .chars()
            .filter(|&ch| ch != '\r' && ch != '\n' && !ch.is_control())
            .collect();
        if filtered.is_empty() {
            return;
        }
        self.query.insert_str(self.cursor, &filtered);
        self.cursor += filtered.len();
        self.current = 0;
    }

    /// Delete the character immediately before the cursor.
    pub fn backspace(&mut self) {
        self.cursor = clamp_char_boundary(&self.query, self.cursor);
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.query, self.cursor);
        self.query.drain(prev..self.cursor);
        self.cursor = prev;
        self.current = 0;
    }

    /// Delete the character immediately at/after the cursor.
    pub fn delete_forward(&mut self) {
        self.cursor = clamp_char_boundary(&self.query, self.cursor);
        if self.cursor >= self.query.len() {
            return;
        }
        let next = next_char_boundary(&self.query, self.cursor);
        self.query.drain(self.cursor..next);
        self.current = 0;
    }

    /// `Option+Backspace`: delete the word (and any immediately trailing whitespace) before the cursor.
    pub fn delete_word_back(&mut self) {
        self.cursor = clamp_char_boundary(&self.query, self.cursor);
        if self.cursor == 0 {
            return;
        }
        let start = word_left_boundary(&self.query, self.cursor);
        self.query.drain(start..self.cursor);
        self.cursor = start;
        self.current = 0;
    }

    /// `Cmd+Backspace`: delete everything from the start of the query up to the cursor.
    pub fn kill_to_start(&mut self) {
        self.cursor = clamp_char_boundary(&self.query, self.cursor);
        if self.cursor == 0 {
            return;
        }
        self.query.drain(..self.cursor);
        self.cursor = 0;
        self.current = 0;
    }

    /// Move the cursor left by one Unicode scalar value.
    pub fn move_left(&mut self) {
        self.cursor = clamp_char_boundary(&self.query, self.cursor);
        self.cursor = prev_char_boundary(&self.query, self.cursor);
    }

    /// Move the cursor right by one Unicode scalar value.
    pub fn move_right(&mut self) {
        self.cursor = clamp_char_boundary(&self.query, self.cursor);
        self.cursor = next_char_boundary(&self.query, self.cursor);
    }

    /// Move the cursor to the beginning of the query.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the query.
    pub fn move_end(&mut self) {
        self.cursor = self.query.len();
    }

    /// Toggle case-sensitive matching and reset the active match index to `0`.
    pub fn toggle_case(&mut self) {
        self.case_sensitive = !self.case_sensitive;
        self.current = 0;
    }

    /// Advance to the next match, wrapping from the last match back to `0`.
    pub fn next_match(&mut self) {
        if self.total_matches == 0 {
            self.current = 0;
            return;
        }
        self.current = (self.current + 1) % self.total_matches;
    }

    /// Move to the previous match, wrapping from `0` to `total_matches - 1`.
    pub fn prev_match(&mut self) {
        if self.total_matches == 0 {
            self.current = 0;
            return;
        }
        if self.current == 0 || self.current >= self.total_matches {
            self.current = self.total_matches - 1;
        } else {
            self.current -= 1;
        }
    }

    /// Update `total_matches` and clamp `current` into `0..total` (or `0` when `total == 0`).
    pub fn clamp_current(&mut self, total: usize) {
        self.total_matches = total;
        if total == 0 {
            self.current = 0;
        } else if self.current >= total {
            self.current = total - 1;
        }
    }

    /// Format the compact match counter label: `""` when query is empty, `"0/0"` when no matches,
    /// or 1-based `"3/14"` otherwise.
    pub fn counter_label(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        if self.total_matches == 0 {
            return "0/0".to_owned();
        }
        let active = self.current.min(self.total_matches - 1) + 1;
        format!("{active}/{}", self.total_matches)
    }
}

/// Logical-pixel bounding box of one non-overlapping match in a [`VisualLine`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FindMatch {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub line_idx: usize,
}

/// Find all non-overlapping matches of `query` across `lines`.
pub fn find_matches_in_lines(
    lines: &[VisualLine],
    m: CellMetrics,
    query: &str,
    case_sensitive: bool,
) -> Vec<FindMatch> {
    if query.is_empty() || !(m.cw > 0.0 && m.ch > 0.0) {
        return Vec::new();
    }

    let mut matches = Vec::new();

    if case_sensitive {
        let needle: Vec<char> = query.chars().collect();
        let nlen = needle.len();
        if nlen == 0 {
            return matches;
        }
        let mut hay: Vec<char> = Vec::new();
        for (line_idx, line) in lines.iter().enumerate() {
            if line.chars.len() < nlen {
                continue;
            }
            hay.clear();
            hay.extend(line.chars.iter().map(|&(_, ch)| ch));

            let mut pos = 0;
            while pos + nlen <= hay.len() {
                if hay[pos..pos + nlen] == needle[..] {
                    let start = pos;
                    let end = pos + nlen;
                    let x = line.chars[start].0;
                    let end_x = line.chars[end - 1].0 + m.cw;
                    let w = (end_x - x).max(m.cw);
                    matches.push(FindMatch {
                        x,
                        y: line.y,
                        w,
                        h: m.ch,
                        line_idx,
                    });
                    pos = end;
                } else {
                    pos += 1;
                }
            }
        }
    } else {
        // Expand each character with `char::to_lowercase` while tracking its original index in
        // `line.chars`, so multi-char lowercase expansions (e.g. `'İ'` -> `['i', '\u{0307}']`)
        // never shift or go out-of-bounds on subsequent `line.chars` indices.
        let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
        let nlen = needle.len();
        if nlen == 0 {
            return matches;
        }
        let mut hay_chars: Vec<char> = Vec::new();
        let mut hay_orig: Vec<usize> = Vec::new();
        for (line_idx, line) in lines.iter().enumerate() {
            if line.chars.is_empty() {
                continue;
            }
            hay_chars.clear();
            hay_orig.clear();
            for (orig_idx, &(_, ch)) in line.chars.iter().enumerate() {
                for lc in ch.to_lowercase() {
                    hay_chars.push(lc);
                    hay_orig.push(orig_idx);
                }
            }
            if hay_chars.len() < nlen {
                continue;
            }

            let mut pos = 0;
            while pos + nlen <= hay_chars.len() {
                if hay_chars[pos..pos + nlen] == needle[..] {
                    let start = hay_orig[pos];
                    let end = hay_orig[pos + nlen - 1] + 1;
                    let x = line.chars[start].0;
                    let end_x = line.chars[end - 1].0 + m.cw;
                    let w = (end_x - x).max(m.cw);
                    matches.push(FindMatch {
                        x,
                        y: line.y,
                        w,
                        h: m.ch,
                        line_idx,
                    });
                    pos += nlen;
                    while pos < hay_orig.len() && hay_orig[pos] < end {
                        pos += 1;
                    }
                } else {
                    pos += 1;
                }
            }
        }
    }

    matches
}

/// Compute the vertical scroll delta to add to `scroll_y` so `m` is visible.
///
/// Returns `0.0` when `m` is already comfortably inside `[view_top + 24.0, view_bottom - 24.0]`.
/// Otherwise places `m.y` near the upper-middle (~38%) of the viewport.
pub fn scroll_delta_for_match(m: &FindMatch, view_top: f32, view_bottom: f32) -> f32 {
    if m.y >= view_top + COMFORT_MARGIN && m.y + m.h <= view_bottom - COMFORT_MARGIN {
        return 0.0;
    }
    let target_y = view_top + (view_bottom - view_top) * TARGET_VIEWPORT_FRACTION;
    m.y - target_y
}

fn clamp_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len()).saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let len = s.len();
    if idx >= len {
        return len;
    }
    let mut i = idx + 1;
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn word_left_boundary(s: &str, cursor: usize) -> usize {
    let mut i = clamp_char_boundary(s, cursor);
    // 1. Skip any whitespace immediately left of cursor.
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        i = p;
    }
    if i == 0 {
        return 0;
    }
    // 2. Delete either a contiguous word-char run or a contiguous punctuation run.
    let p = prev_char_boundary(s, i);
    let first = s[p..i].chars().next().unwrap_or(' ');
    let target_is_word = is_word_char(first);
    i = p;
    while i > 0 {
        let prev = prev_char_boundary(s, i);
        let ch = s[prev..i].chars().next().unwrap_or(' ');
        if ch.is_whitespace() || is_word_char(ch) != target_is_word {
            break;
        }
        i = prev;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(cw: f32, ch: f32) -> CellMetrics {
        CellMetrics {
            cw,
            ch,
            ..CellMetrics::default()
        }
    }

    fn make_line(y: f32, start_x: f32, cw: f32, text: &str) -> VisualLine {
        let chars = text
            .chars()
            .enumerate()
            .map(|(i, ch)| (start_x + i as f32 * cw, ch))
            .collect();
        VisualLine { y, chars }
    }

    #[test]
    fn find_bar_new_and_insert_filters_control_chars() {
        let bar = FindBar::new(None);
        assert_eq!(bar.query, "");
        assert_eq!(bar.cursor, 0);
        assert!(!bar.case_sensitive);
        assert_eq!(bar.current, 0);
        assert_eq!(bar.total_matches, 0);
        assert_eq!(bar.counter_label(), "");

        let mut bar = FindBar::new(Some("foo\r\nbar\x00baz"));
        assert_eq!(bar.query, "foobarbaz");
        assert_eq!(bar.cursor, 9);

        bar.move_home();
        bar.move_right();
        bar.move_right();
        bar.move_right();
        bar.insert_str("\r\n");
        assert_eq!(bar.query, "foobarbaz");
        assert_eq!(bar.cursor, 3);

        bar.insert_str("-\n");
        assert_eq!(bar.query, "foo-barbaz");
        assert_eq!(bar.cursor, 4);
    }

    #[test]
    fn find_bar_editing_and_utf8_cursor_movement() {
        let mut bar = FindBar::new(Some("café🦀"));
        assert_eq!(bar.cursor, bar.query.len());

        // Move left across multi-byte chars: '🦀' (4 bytes), 'é' (2 bytes).
        bar.move_left();
        assert_eq!(&bar.query[bar.cursor..], "🦀");
        bar.move_left();
        assert_eq!(&bar.query[bar.cursor..], "é🦀");

        // Insert in the middle.
        bar.insert_str("X");
        assert_eq!(bar.query, "cafXé🦀");
        assert_eq!(&bar.query[bar.cursor..], "é🦀");

        // Delete forward removes 'é'.
        bar.delete_forward();
        assert_eq!(bar.query, "cafX🦀");
        assert_eq!(&bar.query[bar.cursor..], "🦀");

        // Backspace removes 'X'.
        bar.backspace();
        assert_eq!(bar.query, "caf🦀");
        assert_eq!(&bar.query[bar.cursor..], "🦀");

        // Move home/end and boundary no-ops.
        bar.move_home();
        assert_eq!(bar.cursor, 0);
        bar.move_left();
        assert_eq!(bar.cursor, 0);
        bar.backspace();
        assert_eq!(bar.query, "caf🦀");

        bar.move_end();
        assert_eq!(bar.cursor, bar.query.len());
        bar.move_right();
        assert_eq!(bar.cursor, bar.query.len());
        bar.delete_forward();
        assert_eq!(bar.query, "caf🦀");
    }

    #[test]
    fn find_bar_delete_word_back_and_kill_to_start() {
        let mut bar = FindBar::new(Some("cargo test --release   "));
        bar.delete_word_back();
        assert_eq!(bar.query, "cargo test --");
        bar.delete_word_back();
        assert_eq!(bar.query, "cargo test ");
        bar.delete_word_back();
        assert_eq!(bar.query, "cargo ");
        bar.delete_word_back();
        assert_eq!(bar.query, "");
        bar.delete_word_back();
        assert_eq!(bar.query, "");

        let mut bar = FindBar::new(Some("alpha beta gamma"));
        bar.move_home();
        for _ in 0..10 {
            bar.move_right();
        }
        // Cursor right after "alpha beta"
        bar.kill_to_start();
        assert_eq!(bar.query, " gamma");
        assert_eq!(bar.cursor, 0);
        bar.kill_to_start();
        assert_eq!(bar.query, " gamma");
    }

    #[test]
    fn find_bar_navigation_wrapping_clamp_and_labels() {
        let mut bar = FindBar::new(Some("err"));
        assert_eq!(bar.counter_label(), "0/0");
        bar.next_match();
        assert_eq!(bar.current, 0);
        bar.prev_match();
        assert_eq!(bar.current, 0);

        bar.clamp_current(14);
        assert_eq!(bar.total_matches, 14);
        assert_eq!(bar.current, 0);
        assert_eq!(bar.counter_label(), "1/14");

        bar.next_match();
        bar.next_match();
        assert_eq!(bar.current, 2);
        assert_eq!(bar.counter_label(), "3/14");

        // Wrap backwards from 0 -> 13.
        bar.current = 0;
        bar.prev_match();
        assert_eq!(bar.current, 13);
        assert_eq!(bar.counter_label(), "14/14");

        // Wrap forwards from 13 -> 0.
        bar.next_match();
        assert_eq!(bar.current, 0);
        assert_eq!(bar.counter_label(), "1/14");

        // Clamping to smaller total.
        bar.current = 12;
        bar.clamp_current(5);
        assert_eq!(bar.total_matches, 5);
        assert_eq!(bar.current, 4);
        assert_eq!(bar.counter_label(), "5/5");

        // Toggle case resets current = 0.
        bar.toggle_case();
        assert!(bar.case_sensitive);
        assert_eq!(bar.current, 0);
        bar.toggle_case();
        assert!(!bar.case_sensitive);

        // Clamping to zero.
        bar.clamp_current(0);
        assert_eq!(bar.total_matches, 0);
        assert_eq!(bar.current, 0);
        assert_eq!(bar.counter_label(), "0/0");
    }

    #[test]
    fn find_matches_empty_query_and_invalid_metrics() {
        let m = metrics(8.0, 18.0);
        let lines = vec![make_line(100.0, 12.0, m.cw, "hello world")];

        assert!(find_matches_in_lines(&lines, m, "", false).is_empty());
        assert!(find_matches_in_lines(&lines, metrics(0.0, 18.0), "hello", false).is_empty());
        assert!(find_matches_in_lines(&lines, metrics(8.0, 0.0), "hello", false).is_empty());
        assert!(find_matches_in_lines(&lines, metrics(-4.0, 18.0), "hello", true).is_empty());
    }

    #[test]
    fn find_matches_case_sensitive_vs_insensitive_and_multiple_per_line() {
        let m = metrics(8.0, 18.0);
        let lines = vec![
            make_line(100.0, 10.0, m.cw, "foo Foo FOO foo"),
            make_line(120.0, 10.0, m.cw, "aaaa"),
        ];

        // Case-insensitive: 4 matches on line 0.
        let insens = find_matches_in_lines(&lines, m, "foo", false);
        assert_eq!(insens.len(), 4);
        assert_eq!(
            insens[0],
            FindMatch {
                x: 10.0,
                y: 100.0,
                w: 24.0,
                h: 18.0,
                line_idx: 0,
            }
        );
        assert_eq!(
            insens[1],
            FindMatch {
                x: 10.0 + 4.0 * 8.0,
                y: 100.0,
                w: 24.0,
                h: 18.0,
                line_idx: 0,
            }
        );
        assert_eq!(
            insens[2],
            FindMatch {
                x: 10.0 + 8.0 * 8.0,
                y: 100.0,
                w: 24.0,
                h: 18.0,
                line_idx: 0,
            }
        );
        assert_eq!(
            insens[3],
            FindMatch {
                x: 10.0 + 12.0 * 8.0,
                y: 100.0,
                w: 24.0,
                h: 18.0,
                line_idx: 0,
            }
        );

        // Case-sensitive: only exact lowercase "foo" (indices 0 and 12).
        let sens = find_matches_in_lines(&lines, m, "foo", true);
        assert_eq!(sens.len(), 2);
        assert_eq!(sens[0].x, 10.0);
        assert_eq!(sens[1].x, 10.0 + 12.0 * 8.0);

        // Non-overlapping: "aa" in "aaaa" yields 2 matches, not 3.
        let non_overlap = find_matches_in_lines(&lines, m, "aa", true);
        assert_eq!(non_overlap.len(), 2);
        assert_eq!(non_overlap[0].line_idx, 1);
        assert_eq!(non_overlap[0].x, 10.0);
        assert_eq!(non_overlap[1].x, 10.0 + 2.0 * 8.0);
    }

    #[test]
    fn find_matches_unicode_and_multichar_lowercase_expansion() {
        let m = metrics(10.0, 20.0);
        // Line 0 contains 'İ' (U+0130), whose `char::to_lowercase()` expands to 2 chars ('i' + '\u{0307}').
        // Subsequent characters ("bar") must still align to their exact `line.chars` x positions.
        let lines = vec![
            make_line(50.0, 0.0, m.cw, "İstanbul bar"),
            make_line(70.0, 0.0, m.cw, "CAFÉ café Ωmega ωMEGA 🦀Rust"),
        ];

        // Match "bar" after 'İ' — index 9 in "İstanbul bar".
        let bar_matches = find_matches_in_lines(&lines, m, "bar", false);
        assert_eq!(bar_matches.len(), 1);
        assert_eq!(bar_matches[0].line_idx, 0);
        assert_eq!(bar_matches[0].x, 90.0);
        assert_eq!(bar_matches[0].w, 30.0);

        // Match "İstanbul" case-insensitively.
        let ist = find_matches_in_lines(&lines, m, "İSTANBUL", false);
        assert_eq!(ist.len(), 1);
        assert_eq!(ist[0].x, 0.0);
        assert_eq!(ist[0].w, 80.0);

        // Unicode accented & Greek case-insensitive matching.
        let cafe = find_matches_in_lines(&lines, m, "café", false);
        assert_eq!(cafe.len(), 2);
        assert_eq!(cafe[0].x, 0.0);
        assert_eq!(cafe[0].w, 40.0);
        assert_eq!(cafe[1].x, 50.0);
        assert_eq!(cafe[1].w, 40.0);

        let omega = find_matches_in_lines(&lines, m, "ωmega", false);
        assert_eq!(omega.len(), 2);
        assert_eq!(omega[0].x, 100.0);
        assert_eq!(omega[0].w, 50.0);
        assert_eq!(omega[1].x, 160.0);
        assert_eq!(omega[1].w, 50.0);

        // Emoji + ASCII match.
        let crab = find_matches_in_lines(&lines, m, "🦀rust", false);
        assert_eq!(crab.len(), 1);
        assert_eq!(crab[0].x, 220.0);
        assert_eq!(crab[0].w, 50.0);
    }

    #[test]
    fn scroll_delta_for_match_comfort_zone_and_recenter() {
        let view_top = 40.0;
        let view_bottom = 640.0;
        // target_y = 40.0 + 600.0 * 0.38 = 268.0

        // Comfortably inside [64.0, 616.0] -> no scroll.
        let inside = FindMatch {
            x: 10.0,
            y: 200.0,
            w: 40.0,
            h: 18.0,
            line_idx: 5,
        };
        assert_eq!(scroll_delta_for_match(&inside, view_top, view_bottom), 0.0);

        // Exact top and bottom comfort edges -> still 0.0.
        let top_edge = FindMatch {
            x: 10.0,
            y: 64.0,
            w: 40.0,
            h: 18.0,
            line_idx: 1,
        };
        assert_eq!(scroll_delta_for_match(&top_edge, view_top, view_bottom), 0.0);

        let bottom_edge = FindMatch {
            x: 10.0,
            y: 598.0,
            w: 40.0,
            h: 18.0,
            line_idx: 20,
        };
        assert_eq!(
            scroll_delta_for_match(&bottom_edge, view_top, view_bottom),
            0.0
        );

        // Above comfort zone -> negative delta (scroll up).
        let above = FindMatch {
            x: 10.0,
            y: 50.0,
            w: 40.0,
            h: 18.0,
            line_idx: 0,
        };
        let d_above = scroll_delta_for_match(&above, view_top, view_bottom);
        assert!((d_above - (50.0 - 268.0)).abs() < 1e-4);

        // Below viewport -> positive delta (scroll down).
        let below = FindMatch {
            x: 10.0,
            y: 900.0,
            w: 40.0,
            h: 18.0,
            line_idx: 40,
        };
        let d_below = scroll_delta_for_match(&below, view_top, view_bottom);
        assert!((d_below - (900.0 - 268.0)).abs() < 1e-4);
    }
}
