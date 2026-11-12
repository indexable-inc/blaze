//! The reference model: a terminal as the screen's bytes need one, small enough to read. It
//! keeps the lines written as logical lines and wraps them at its width when asked what it
//! shows, so a narrower terminal reflows what it holds, as terminals do. It knows the
//! sequences the encoder writes and refuses any other, so a test that reads a screen from it
//! has also checked that nothing else was written.
//!
//! What it models of a real terminal, each a way stale rows are left:
//! - the cursor moves up by rows as they are wrapped now, not as they were written;
//! - it never moves above the top of the screen: a region taller than the terminal cannot be
//!   reached to be erased;
//! - erasing is from the cursor to the end of the screen.

#![allow(dead_code, reason = "each test file uses a part of it")]

pub(crate) struct Vt {
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    /// Each logical line; the cursor is in the last or an earlier one.
    pub(crate) lines: Vec<String>,
    /// The cursor: its logical line and its column in it, in characters.
    line: usize,
    col: usize,
    pub(crate) cursor_hidden: bool,
    pub(crate) synchronized: bool,
    /// Each OSC written, without its introducer and terminator.
    pub(crate) osc: Vec<String>,
    /// Whether any escape sequence was written.
    pub(crate) escapes: usize,
}

impl Vt {
    pub(crate) fn new(cols: u16, rows: u16) -> Vt {
        Vt {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
            lines: vec![String::new()],
            line: 0,
            col: 0,
            cursor_hidden: false,
            synchronized: false,
            osc: Vec::new(),
            escapes: 0,
        }
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = usize::from(cols.max(1));
        self.rows = usize::from(rows.max(1));
    }

    /// The rows a logical line of `len` characters takes.
    fn rows_of(&self, len: usize) -> usize {
        len.div_ceil(self.cols).max(1)
    }

    /// The row the cursor is on, counted from the first line ever written.
    fn cursor_row(&self) -> usize {
        let above: usize =
            self.lines[..self.line].iter().map(|it| self.rows_of(it.chars().count())).sum();
        above + self.col / self.cols
    }

    fn total_rows(&self) -> usize {
        self.lines.iter().map(|it| self.rows_of(it.chars().count())).sum()
    }

    fn up(&mut self, n: usize) {
        // The top row still on screen: what scrolled off cannot be reached.
        let top = self.total_rows().saturating_sub(self.rows);
        let target = self.cursor_row().saturating_sub(n).max(top);
        let within = self.col % self.cols;
        let mut row = 0;
        for (at, line) in self.lines.iter().enumerate() {
            let rows = self.rows_of(line.chars().count());
            if target < row + rows {
                self.line = at;
                self.col = (target - row) * self.cols + within;
                return;
            }
            row += rows;
        }
    }

    fn erase_below(&mut self) {
        let keep: String = self.lines[self.line].chars().take(self.col).collect();
        self.lines[self.line] = keep;
        self.lines.truncate(self.line + 1);
    }

    fn put(&mut self, c: char) {
        let line = &mut self.lines[self.line];
        let len = line.chars().count();
        if self.col < len {
            let mut chars: Vec<char> = line.chars().collect();
            chars[self.col] = c;
            *line = chars.into_iter().collect();
        } else {
            for _ in len..self.col {
                line.push(' ');
            }
            line.push(c);
        }
        self.col += 1;
    }

    /// Takes what a driver wrote. Panics on a sequence the encoder has no business writing.
    pub(crate) fn write(&mut self, bytes: &str) {
        let mut chars = bytes.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\n' => {
                    self.line += 1;
                    self.col = 0;
                    if self.line == self.lines.len() {
                        self.lines.push(String::new());
                    }
                }
                '\r' => self.col -= self.col % self.cols,
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
                                ('J', "" | "0") => self.erase_below(),
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

    /// Every logical line written and still held, without the empty one the cursor waits on.
    pub(crate) fn held(&self) -> Vec<&str> {
        let mut lines: Vec<&str> = self.lines.iter().map(String::as_str).collect();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        lines
    }

    /// What the terminal shows: its last rows, each line wrapped at its width.
    pub(crate) fn shown(&self) -> String {
        let mut rows: Vec<String> = Vec::new();
        for line in self.held() {
            let chars: Vec<char> = line.chars().collect();
            if chars.is_empty() {
                rows.push(String::new());
            }
            for chunk in chars.chunks(self.cols) {
                rows.push(chunk.iter().collect::<String>().trim_end().to_owned());
            }
        }
        let from = rows.len().saturating_sub(self.rows);
        rows[from..].join("\n")
    }

    /// The terminal's progress report as written last: OSC 9;4's state and percentage.
    pub(crate) fn progress(&self) -> Option<&str> {
        self.osc.iter().rev().find_map(|it| it.strip_prefix("9;4;"))
    }
}
