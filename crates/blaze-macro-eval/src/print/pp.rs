//! rustc's Oppen printer (`rustc_ast_pretty::pp` and `pp/ring.rs`), line for line, with the
//! only change that a string token is a range of the printer's own text buffer instead of a
//! `Cow`, so that printing allocates nothing once the buffers have grown. Every buffer is kept
//! across prints ([`Printer::reset`]).

use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Breaks {
    Consistent,
    Inconsistent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IndentStyle {
    Visual,
    Block { offset: isize },
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct BreakToken {
    offset: isize,
    blank_space: isize,
    pre_break: Option<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BeginToken {
    indent: IndentStyle,
    breaks: Breaks,
}

/// A string is `start..start + len` of [`Printer::text`]; its size is its length in bytes, as
/// rustc's is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token {
    String { start: u32, len: u32 },
    Break(BreakToken),
    Begin(BeginToken),
    End,
}

const HARDBREAK: Token =
    Token::Break(BreakToken { offset: 0, blank_space: SIZE_INFINITY, pre_break: None });

impl Token {
    fn is_hardbreak_tok(&self) -> bool {
        *self == HARDBREAK
    }
}

#[derive(Clone, Copy, Debug)]
enum PrintFrame {
    Fits,
    Broken { indent: usize, breaks: Breaks },
}

const SIZE_INFINITY: isize = 0xffff;

/// Target line width.
const MARGIN: isize = 78;
/// Every line is allowed at least this much space, even if highly indented.
const MIN_SPACE: isize = 60;

#[derive(Clone, Copy, Debug)]
struct BufEntry {
    token: Token,
    size: isize,
}

/// rustc's `RingBuffer`: a window `offset..offset + data.len()` of an infinite sequence.
/// `clear` keeps `offset`, as rustc's does.
#[derive(Default)]
struct Ring {
    data: VecDeque<BufEntry>,
    offset: usize,
}

impl Ring {
    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn push(&mut self, value: BufEntry) -> usize {
        let index = self.offset + self.data.len();
        self.data.push_back(value);
        index
    }

    fn clear(&mut self) {
        self.data.clear();
    }

    fn index_of_first(&self) -> usize {
        self.offset
    }

    fn first(&self) -> Option<&BufEntry> {
        self.data.front()
    }

    fn first_mut(&mut self) -> Option<&mut BufEntry> {
        self.data.front_mut()
    }

    fn pop_first(&mut self) -> Option<BufEntry> {
        let first = self.data.pop_front()?;
        self.offset += 1;
        Some(first)
    }

    fn last(&self) -> Option<&BufEntry> {
        self.data.back()
    }

    fn last_mut(&mut self) -> Option<&mut BufEntry> {
        self.data.back_mut()
    }

    fn get_mut(&mut self, index: usize) -> &mut BufEntry {
        &mut self.data[index.checked_sub(self.offset).expect("pp: index left of the ring")]
    }
}

/// The printer's state. Its output goes to the `out` each call is given, which only ever
/// grows: the algorithm never reads what it wrote.
#[derive(Default)]
pub(super) struct Printer {
    /// Number of spaces left on line.
    space: isize,
    buf: Ring,
    /// Running size of stream "...left".
    left_total: isize,
    /// Running size of stream "...right".
    right_total: isize,
    /// The ring indices of the `Begin` that started the current block, possibly with the most
    /// recent `Break` after it on top.
    scan_stack: VecDeque<usize>,
    print_stack: Vec<PrintFrame>,
    /// Level of indentation of the current line.
    indent: usize,
    /// Indentation not yet written, so that no line ends in spaces.
    pending_indentation: isize,
    /// The token most recently printed off the left of the ring.
    last_printed: Option<Token>,
    /// The text of the string tokens in the ring.
    text: String,
}

impl Printer {
    /// The state of rustc's `Printer::new()`, keeping every buffer's capacity.
    pub(super) fn reset(&mut self) {
        self.space = MARGIN;
        self.buf.clear();
        self.buf.offset = 0;
        self.left_total = 0;
        self.right_total = 0;
        self.scan_stack.clear();
        self.print_stack.clear();
        self.indent = 0;
        self.pending_indentation = 0;
        self.last_printed = None;
        self.text.clear();
    }

    fn last_token(&self) -> Option<Token> {
        self.last_token_still_buffered().or(self.last_printed)
    }

    fn last_token_still_buffered(&self) -> Option<Token> {
        self.buf.last().map(|last| last.token)
    }

    fn replace_last_token_still_buffered(&mut self, token: Token) {
        self.buf.last_mut().expect("pp: a buffered token").token = token;
    }

    /// The ring is empty and no string in it is still needed, so its text can go.
    fn clear_buf(&mut self) {
        self.buf.clear();
        self.text.clear();
    }

    fn scan_eof(&mut self, out: &mut String) {
        if !self.scan_stack.is_empty() {
            self.check_stack(0);
            self.advance_left(out);
        }
    }

    fn scan_begin(&mut self, token: BeginToken) {
        if self.scan_stack.is_empty() {
            self.left_total = 1;
            self.right_total = 1;
            self.clear_buf();
        }
        let right = self.buf.push(BufEntry { token: Token::Begin(token), size: -self.right_total });
        self.scan_stack.push_back(right);
    }

    fn scan_end(&mut self) {
        if self.scan_stack.is_empty() {
            self.print_end();
        } else {
            let right = self.buf.push(BufEntry { token: Token::End, size: -1 });
            self.scan_stack.push_back(right);
        }
    }

    fn scan_break(&mut self, token: BreakToken) {
        if self.scan_stack.is_empty() {
            self.left_total = 1;
            self.right_total = 1;
            self.clear_buf();
        } else {
            self.check_stack(0);
        }
        let right = self.buf.push(BufEntry { token: Token::Break(token), size: -self.right_total });
        self.scan_stack.push_back(right);
        self.right_total += token.blank_space;
    }

    fn scan_string(&mut self, string: &str, out: &mut String) {
        if self.scan_stack.is_empty() {
            self.print_string(string, out);
        } else {
            let len = string.len() as isize;
            let start = self.text.len() as u32;
            self.text.push_str(string);
            self.buf.push(BufEntry {
                token: Token::String { start, len: string.len() as u32 },
                size: len,
            });
            self.right_total += len;
            self.check_stream(out);
        }
    }

    fn check_stream(&mut self, out: &mut String) {
        while self.right_total - self.left_total > self.space {
            if *self.scan_stack.front().expect("pp: a scanned token") == self.buf.index_of_first() {
                self.scan_stack.pop_front();
                self.buf.first_mut().expect("pp: a buffered token").size = SIZE_INFINITY;
            }
            self.advance_left(out);
            if self.buf.is_empty() {
                break;
            }
        }
    }

    fn advance_left(&mut self, out: &mut String) {
        while self.buf.first().expect("pp: a buffered token").size >= 0 {
            let left = self.buf.pop_first().expect("pp: a buffered token");

            match left.token {
                Token::String { start, len } => {
                    self.left_total += len as isize;
                    let (start, end) = (start as usize, (start + len) as usize);
                    print_string(
                        &self.text[start..end],
                        out,
                        &mut self.pending_indentation,
                        &mut self.space,
                    );
                }
                Token::Break(token) => {
                    self.left_total += token.blank_space;
                    self.print_break(token, left.size, out);
                }
                Token::Begin(token) => self.print_begin(token, left.size),
                Token::End => self.print_end(),
            }

            self.last_printed = Some(left.token);

            if self.buf.is_empty() {
                break;
            }
        }
    }

    fn check_stack(&mut self, mut depth: usize) {
        while let Some(&index) = self.scan_stack.back() {
            let right_total = self.right_total;
            let entry = self.buf.get_mut(index);
            match entry.token {
                Token::Begin(_) => {
                    if depth == 0 {
                        break;
                    }
                    self.scan_stack.pop_back();
                    entry.size += right_total;
                    depth -= 1;
                }
                Token::End => {
                    // rustc's: the paper says + not =, but that makes no sense.
                    self.scan_stack.pop_back();
                    entry.size = 1;
                    depth += 1;
                }
                Token::String { .. } | Token::Break(_) => {
                    self.scan_stack.pop_back();
                    entry.size += right_total;
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
    }

    fn get_top(&self) -> PrintFrame {
        *self
            .print_stack
            .last()
            .unwrap_or(&PrintFrame::Broken { indent: 0, breaks: Breaks::Inconsistent })
    }

    fn print_begin(&mut self, token: BeginToken, size: isize) {
        if size > self.space {
            self.print_stack.push(PrintFrame::Broken { indent: self.indent, breaks: token.breaks });
            self.indent = match token.indent {
                IndentStyle::Block { offset } => usize::try_from(self.indent as isize + offset)
                    .expect("pp: indentation below zero"),
                IndentStyle::Visual => (MARGIN - self.space) as usize,
            };
        } else {
            self.print_stack.push(PrintFrame::Fits);
        }
    }

    fn print_end(&mut self) {
        if let PrintFrame::Broken { indent, .. } =
            self.print_stack.pop().expect("pp: an open box")
        {
            self.indent = indent;
        }
    }

    fn print_break(&mut self, token: BreakToken, size: isize, out: &mut String) {
        let fits = match self.get_top() {
            PrintFrame::Fits => true,
            PrintFrame::Broken { breaks: Breaks::Consistent, .. } => false,
            PrintFrame::Broken { breaks: Breaks::Inconsistent, .. } => size <= self.space,
        };
        if fits {
            self.pending_indentation += token.blank_space;
            self.space -= token.blank_space;
        } else {
            if let Some(pre_break) = token.pre_break {
                out.push(pre_break as char);
            }
            out.push('\n');
            let indent = self.indent as isize + token.offset;
            self.pending_indentation = indent;
            self.space = MIN_SPACE.max(MARGIN - indent);
        }
    }

    fn print_string(&mut self, string: &str, out: &mut String) {
        print_string(string, out, &mut self.pending_indentation, &mut self.space);
    }

    /// "raw box"
    pub(super) fn rbox(&mut self, indent: isize, breaks: Breaks) {
        self.scan_begin(BeginToken { indent: IndentStyle::Block { offset: indent }, breaks })
    }

    /// Inconsistent breaking box.
    pub(super) fn ibox(&mut self, indent: isize) {
        self.rbox(indent, Breaks::Inconsistent)
    }

    /// Consistent breaking box.
    pub(super) fn cbox(&mut self, indent: isize) {
        self.rbox(indent, Breaks::Consistent)
    }

    pub(super) fn break_offset(&mut self, n: usize, off: isize) {
        self.scan_break(BreakToken { offset: off, blank_space: n as isize, pre_break: None });
    }

    pub(super) fn end(&mut self) {
        self.scan_end()
    }

    pub(super) fn eof(&mut self, out: &mut String) {
        self.scan_eof(out);
    }

    pub(super) fn word(&mut self, word: &str, out: &mut String) {
        self.scan_string(word, out)
    }

    pub(super) fn space(&mut self) {
        self.break_offset(1, 0)
    }

    pub(super) fn hardbreak(&mut self) {
        self.break_offset(SIZE_INFINITY as usize, 0)
    }

    pub(super) fn is_beginning_of_line(&self) -> bool {
        match self.last_token() {
            Some(last_token) => last_token.is_hardbreak_tok(),
            None => true,
        }
    }

    /// `pprust::State::break_offset_if_not_bol`.
    pub(super) fn break_offset_if_not_bol(&mut self, n: usize, off: isize) {
        if !self.is_beginning_of_line() {
            self.break_offset(n, off)
        } else if off != 0
            && let Some(last_token) = self.last_token_still_buffered()
            && last_token.is_hardbreak_tok()
        {
            // rustc's: tuck the offset meant for the break into the previous hardbreak.
            self.replace_last_token_still_buffered(Token::Break(BreakToken {
                offset: off,
                blank_space: SIZE_INFINITY,
                pre_break: None,
            }));
        }
    }
}

/// Writes the pending indentation, then `string`.
fn print_string(string: &str, out: &mut String, pending_indentation: &mut isize, space: &mut isize) {
    let pending = *pending_indentation as usize;
    out.reserve(pending + string.len());
    out.extend(std::iter::repeat_n(' ', pending));
    *pending_indentation = 0;
    out.push_str(string);
    *space -= string.len() as isize;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: impl FnOnce(&mut Printer, &mut String)) -> String {
        let mut printer = Printer::default();
        printer.reset();
        let mut out = String::new();
        f(&mut printer, &mut out);
        printer.eof(&mut out);
        out
    }

    #[test]
    fn a_line_that_fits_is_printed_as_is() {
        let out = run(|p, out| {
            p.word("a", out);
            p.space();
            p.word("b", out);
        });
        assert_eq!(out, "a b");
    }

    #[test]
    fn an_inconsistent_box_breaks_only_where_the_line_is_full() {
        // 30 words of 4 bytes and a space: a line holds 15 of them (15 * 5 - 1 = 74 <= 78).
        let out = run(|p, out| {
            p.ibox(0);
            for i in 0..30 {
                if i > 0 {
                    p.space();
                }
                p.word("abcd", out);
            }
            p.end();
        });
        let line = ["abcd"; 15].join(" ");
        assert_eq!(out, format!("{line}\n{line}"));
    }

    #[test]
    fn a_consistent_box_that_does_not_fit_breaks_at_every_break() {
        let out = run(|p, out| {
            p.cbox(4);
            p.word("{", out);
            p.space();
            p.ibox(0);
            p.word(&"x".repeat(80), out);
            p.end();
            p.break_offset_if_not_bol(1, -4);
            p.word("}", out);
            p.end();
        });
        assert_eq!(out, format!("{{\n    {}\n}}", "x".repeat(80)));
    }

    #[test]
    fn a_hardbreak_is_the_beginning_of_a_line() {
        let mut p = Printer::default();
        p.reset();
        let mut out = String::new();
        assert!(p.is_beginning_of_line());
        p.ibox(0);
        p.word("a", &mut out);
        assert!(!p.is_beginning_of_line());
        p.hardbreak();
        assert!(p.is_beginning_of_line());
        p.word("b", &mut out);
        p.end();
        p.eof(&mut out);
        assert_eq!(out, "a\nb");
    }
}
