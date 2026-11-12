//! A terminal as the screen's bytes need one, small enough to read ([`Vt`]). It
//! keeps rows as a terminal does, each knowing whether it wrapped into the next, and reflows
//! them when its width changes, as terminals do. It knows the sequences the encoder writes
//! and refuses any other, so a test that reads a screen from it has also checked that nothing
//! else was written.
//!
//! What it models of a real terminal, each a way stale rows are left:
//! - the cursor moves up by rows as they are wrapped now, not as they were written;
//! - it never moves above the top of the screen: a region taller than the terminal cannot be
//!   reached to be erased;
//! - erasing a row (EL) erases that row alone, and writing over a row leaves what the new
//!   text does not cover;
//! - a terminal made shorter gives up the blank rows under the cursor first, then rows off
//!   its top.
//!
//! Erasing to the end of the screen (ED) is refused: tmux takes one from the top-left corner
//! for a clear of the whole screen and pushes the screen into its history first, so a region
//! redrawn with it is copied into the scrollback a frame (seen on 2026-11-12 in tmux 3.8).

#![allow(dead_code, reason = "each test file uses a part of it")]

/// The reference model: a terminal that takes the encoder's sequences and no other, keeps
/// rows with their wraps, reflows them when resized, never moves its cursor above its top row,
/// and erases a row only when told to: what the screen's properties read a terminal's state
/// from.
pub(crate) struct Vt {
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    /// Each row written, from the first: its text, and whether it wrapped into the next.
    screen: Vec<(String, bool)>,
    /// The cursor: its row and its column.
    row: usize,
    col: usize,
    pub(crate) cursor_hidden: bool,
    pub(crate) synchronized: bool,
    /// Each OSC written, without its introducer and terminator.
    pub(crate) osc: Vec<String>,
    /// How many escape sequences were written.
    pub(crate) escapes: usize,
}

impl Vt {
    pub(crate) fn new(cols: u16, rows: u16) -> Vt {
        Vt {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
            screen: vec![(String::new(), false)],
            row: 0,
            col: 0,
            cursor_hidden: false,
            synchronized: false,
            osc: Vec::new(),
            escapes: 0,
        }
    }

    /// The terminal resized: every line is wrapped again at the new width, and the cursor
    /// stays on the character it was on.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        let cols = usize::from(cols.max(1));
        // The cursor as a line and an offset in it.
        let mut lines: Vec<String> = vec![String::new()];
        let mut at = (0, 0);
        for (index, (text, wraps)) in self.screen.iter().enumerate() {
            let line = lines.len() - 1;
            if index == self.row {
                at = (line, lines[line].chars().count() + self.col);
            }
            lines[line].push_str(text);
            if *wraps {
                // A row that wrapped was full.
                let short = self.cols.saturating_sub(text.chars().count());
                lines[line].extend(std::iter::repeat_n(' ', short));
            } else if index + 1 < self.screen.len() {
                lines.push(String::new());
            }
        }
        self.cols = cols;
        self.rows = usize::from(rows.max(1));
        self.screen.clear();
        for (line, text) in lines.iter().enumerate() {
            let chars: Vec<char> = text.chars().collect();
            let chunks: Vec<&[char]> = match chars.is_empty() {
                true => vec![&[][..]],
                false => chars.chunks(cols).collect(),
            };
            if line == at.0 {
                let offset = at.1.min(chars.len());
                self.row = self.screen.len() + (offset / cols).min(chunks.len() - 1);
                self.col = offset - (self.row - self.screen.len()) * cols;
            }
            let last = chunks.len() - 1;
            for (index, chunk) in chunks.into_iter().enumerate() {
                self.screen.push((chunk.iter().collect(), index < last));
            }
        }
        // A terminal that gets shorter gives up the blank rows under the cursor before it
        // pushes anything off its top (tmux's `screen_resize_y`; VTE and kitty likewise).
        // xterm cuts rows from the bottom whatever they hold, which this does not model.
        while self.screen.len() > self.row + 1 && self.screen.last().is_some_and(|it| it.0.trim().is_empty()) {
            self.screen.pop();
        }
    }

    fn up(&mut self, n: usize) {
        // The top row still on screen: what scrolled off cannot be reached.
        let top = self.screen.len().saturating_sub(self.rows);
        self.row = self.row.saturating_sub(n).max(top);
    }

    fn put(&mut self, c: char) {
        if self.col >= self.cols {
            self.screen[self.row].1 = true;
            self.row += 1;
            self.col = 0;
            if self.row == self.screen.len() {
                self.screen.push((String::new(), false));
            }
        }
        let mut chars: Vec<char> = self.screen[self.row].0.chars().collect();
        while chars.len() <= self.col {
            chars.push(' ');
        }
        chars[self.col] = c;
        self.screen[self.row].0 = chars.into_iter().collect();
        self.col += 1;
    }

    /// Takes what a driver wrote. Panics on a sequence the encoder has no business writing.
    pub(crate) fn write(&mut self, bytes: &str) {
        let mut chars = bytes.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\n' => {
                    self.row += 1;
                    self.col = 0;
                    if self.row == self.screen.len() {
                        self.screen.push((String::new(), false));
                    }
                }
                '\r' => self.col = 0,
                '\x1b' => {
                    self.escapes += 1;
                    match chars.next() {
                        Some('[') => {
                            let mut params = String::new();
                            let last = loop {
                                match chars.next() {
                                    Some(c) if c.is_ascii_digit() || c == ';' || c == '?' => params.push(c),
                                    Some(c) => break c,
                                    None => panic!("a CSI with no end: {params:?}"),
                                }
                            };
                            match (last, params.as_str()) {
                                ('A', n) => self.up(n.parse().unwrap_or(1)),
                                ('K', "2") => self.screen[self.row] = (String::new(), false),
                                ('m', _) => {}
                                ('h', "?25") => self.cursor_hidden = false,
                                ('l', "?25") => self.cursor_hidden = true,
                                ('h', "?2026") => self.synchronized = true,
                                ('l', "?2026") => self.synchronized = false,
                                other => panic!("a sequence the encoder does not write: CSI {other:?}"),
                            }
                        }
                        Some(']') => {
                            let mut text = String::new();
                            loop {
                                match chars.next() {
                                    Some('\x07') => break,
                                    Some('\x1b') if chars.peek() == Some(&'\\') => {
                                        chars.next();
                                        break;
                                    }
                                    Some(c) => text.push(c),
                                    None => panic!("an OSC with no end: {text:?}"),
                                }
                            }
                            self.osc.push(text);
                        }
                        other => panic!("a sequence the encoder does not write: ESC {other:?}"),
                    }
                }
                c if c.is_control() => panic!("a control character the encoder does not write: {c:?}"),
                c => self.put(c),
            }
        }
    }

    /// Every line the terminal holds, a row that wrapped joined with the next, without the
    /// blank ones at its end (rows erased and not written again, and the one the cursor waits
    /// on).
    pub(crate) fn held(&self) -> Vec<String> {
        let mut lines: Vec<String> = vec![String::new()];
        for (index, (text, wraps)) in self.screen.iter().enumerate() {
            let line = lines.len() - 1;
            lines[line].push_str(text);
            if *wraps {
                let short = self.cols.saturating_sub(text.chars().count());
                lines[line].extend(std::iter::repeat_n(' ', short));
            } else if index + 1 < self.screen.len() {
                lines.push(String::new());
            }
        }
        while lines.last().is_some_and(|it| it.trim().is_empty()) {
            lines.pop();
        }
        lines
    }

    /// What the terminal shows: its last rows.
    pub(crate) fn shown(&self) -> String {
        let mut rows: Vec<&str> = self.screen.iter().map(|it| it.0.trim_end()).collect();
        while rows.last() == Some(&"") {
            rows.pop();
        }
        let from = rows.len().saturating_sub(self.rows);
        rows[from..].join("\n")
    }

    /// The terminal's progress report as written last: OSC 9;4's state and percentage.
    pub(crate) fn progress(&self) -> Option<&str> {
        self.osc.iter().rev().find_map(|it| it.strip_prefix("9;4;"))
    }
}
