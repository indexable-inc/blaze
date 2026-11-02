//! Where the lines of a text are (docs/v2/plan/design/frontend.md item 6,
//! docs/v2/plan/lanes/frontend/text.md): built only when something asks for a line or a
//! column (a diagnostic, `line!`/`column!`, a caller location, debug info), never on load.
//!
//! [`LineIndex`] is a bitmap of the text's `\n` bytes (`simd`, 64 bytes per word with the
//! widest vector unit the CPU has) and a rank table: the newlines before each 512 bytes, one
//! 4-byte count per 64-byte cache line of bits (rank9's layout, without its relative counts).
//! 0.133 bytes per byte of text. An offset's line is one rank: a load of the table and at most
//! eight popcounts of one cache line. Its line's start is a backward scan of that cache line,
//! or a select. A line's start is one select: a binary search of the table, then popcounts
//! of one cache line and a bit select in a word.
//!
//! Lines and columns are rustc's (`rustc_span::SourceFile`, `SourceMap::lookup_char_pos`):
//! - a line ends at each `\n`, but a `\n` that ends the text starts no line: the text's end is
//!   on the line that `\n` ends, which counts it as a char (and a byte) but not as width;
//! - rustc drops a byte order mark that starts the file, so no column counts it;
//! - rustc reads `\r\n` as `\n`, so a `\r` before a `\n` is no column of the `\n`. Every other
//!   `\r` stays, and the bytes and chars before a place are the same either way;
//! - columns are from 1, and [`Place`] gives each kind rustc counts: chars (`column!`, a
//!   diagnostic's `file:line:col`, `proc_macro::Span::column`), display width
//!   (`Location::caller`, rustc's `col_display`), and bytes (DWARF's line table).

#[allow(unsafe_code)]
mod simd;
#[cfg(test)]
mod tests;

use std::fmt;

use blaze_ids::bits::below;
use text_size::TextSize;
use unicode_width::UnicodeWidthChar;

pub use self::simd::Kernel;

/// Words of bits per rank entry: 512 bytes of text, one cache line of bits.
const BLOCK: usize = 8;

/// The newlines of a text, for its lines and columns. Built in one pass over the text; holds
/// no copy of it, so a [`Place`]'s columns are asked of the text it was built from.
#[derive(PartialEq, Eq)]
pub struct LineIndex {
    /// Bit `i % 64` of word `i / 64` is set when byte `i` is `\n`. 1 bit a byte. Ranked a
    /// block of [`BLOCK`] words at a time, not a word at a time as a `blaze_ids::bits::Ranked`.
    bits: Box<[u64]>,
    /// The newlines before each [`BLOCK`] words, then all of them. 4 bytes per 512 of text.
    ranks: Box<[u32]>,
    len: u32,
    /// The length of the byte order mark the text starts with: 3, or 0 for none.
    bom: u32,
}

impl fmt::Debug for LineIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineIndex").field("len", &self.len).field("lines", &self.line_count()).finish()
    }
}

impl LineIndex {
    /// The index of `text`, with the fastest [`Kernel`] this CPU has. Panics on a text of
    /// 4 GiB or more: offsets are `u32`, as [`TextSize`] is.
    pub fn new(text: &str) -> LineIndex {
        LineIndex::build(text, Kernel::best())
    }

    /// The index of `text` made with `kernel`; `None` when this CPU can't run it. Every kernel
    /// makes the same index.
    pub fn with_kernel(text: &str, kernel: Kernel) -> Option<LineIndex> {
        kernel.available().then(|| LineIndex::build(text, kernel))
    }

    fn build(text: &str, kernel: Kernel) -> LineIndex {
        let bytes = text.as_bytes();
        let Ok(len) = u32::try_from(bytes.len()) else {
            panic!("a text of {} bytes: a line index's offsets are 32 bits", bytes.len());
        };
        // alloc: the index's bitmap, its result, once per text, at its exact length
        let mut bits = vec![0u64; bytes.len().div_ceil(64)].into_boxed_slice();
        simd::fill(kernel, bytes, &mut bits);
        let mut ranks = Vec::with_capacity(bits.len().div_ceil(BLOCK) + 1);
        let mut before = 0u32;
        for block in bits.chunks(BLOCK) {
            ranks.push(before);
            before += block.iter().map(|it| it.count_ones()).sum::<u32>();
        }
        ranks.push(before);
        let bom = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() as u32 } else { 0 };
        LineIndex { bits, ranks: ranks.into_boxed_slice(), len, bom }
    }

    /// The length of the text it was built from.
    pub fn len(&self) -> TextSize {
        TextSize::new(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The text's lines: one more than its newlines, less one when the text ends with one. An
    /// empty text is one empty line here, for what shows lines; its [`Place`] says rustc's 0.
    pub fn line_count(&self) -> u32 {
        self.newlines() + 1 - u32::from(self.ends_with_newline())
    }

    /// The heap the index holds, in bytes.
    pub fn heap_bytes(&self) -> usize {
        size_of_val(&*self.bits) + size_of_val(&*self.ranks)
    }

    fn newlines(&self) -> u32 {
        self.ranks[self.ranks.len() - 1]
    }

    /// Whether the last byte is `\n`: rustc's `analyze_source_file` drops the line start that
    /// would follow it, at the text's end.
    fn ends_with_newline(&self) -> bool {
        let Some(last) = (self.len as usize).checked_sub(1) else { return false };
        self.bits[last / 64] >> (last % 64) & 1 == 1
    }

    /// The line `offset` is on, from 0: the newlines before it, but the text's end after a
    /// last `\n` is on the line that `\n` ends. `None` past the text's end.
    pub fn line(&self, offset: TextSize) -> Option<u32> {
        let at = u32::from(offset);
        (at <= self.len).then(|| self.rank(at as usize) - u32::from(self.at_end_past_newline(at)))
    }

    fn at_end_past_newline(&self, at: u32) -> bool {
        at == self.len && self.ends_with_newline()
    }

    /// Where `line` (from 0) starts: 0, or just past the newline that ends the line before.
    /// `None` for a line the text lacks.
    pub fn line_start(&self, line: u32) -> Option<TextSize> {
        if line >= self.line_count() {
            return None;
        }
        Some(TextSize::new(match line {
            0 => 0,
            _ => self.select(line - 1) + 1,
        }))
    }

    /// Where `line` (from 0) ends: at its newline, or at the text's end for a last line with
    /// none. `None` for a line the text lacks.
    pub fn line_end(&self, line: u32) -> Option<TextSize> {
        if line >= self.line_count() {
            return None;
        }
        Some(TextSize::new(match line < self.newlines() {
            true => self.select(line),
            false => self.len,
        }))
    }

    /// Where `offset` is: its line and where the line's columns start. `None` past the
    /// text's end.
    pub fn place(&self, offset: TextSize) -> Option<Place> {
        let at = u32::from(offset);
        if at > self.len {
            return None;
        }
        let past_newline = self.at_end_past_newline(at);
        // Past a last `\n`, the line is the one it ends, found from just before it.
        let before = if past_newline { at - 1 } else { at };
        let line = self.rank(at as usize) - u32::from(past_newline);
        let start = match line {
            // An offset inside the byte order mark is where the text starts.
            0 => self.bom.min(at),
            _ => self.start_before(before as usize, line),
        };
        // rustc has no line in a text with nothing after its byte order mark, and says line 0.
        let line = if self.len == self.bom { 0 } else { line + 1 };
        Some(Place { line, start, offset: at, past_newline })
    }

    /// The row of `offset` in a text that is part of a file (a body's text), from 0, and where
    /// the row starts: the newlines before it, and just past the last of them. None of
    /// [`LineIndex::place`]'s whole-file rules apply: a leading U+FEFF is a char, and the end
    /// past a last `\n` starts a row. `None` past the text's end.
    pub fn row(&self, offset: TextSize) -> Option<(u32, TextSize)> {
        let at = u32::from(offset);
        if at > self.len {
            return None;
        }
        let row = self.rank(at as usize);
        let start = if row == 0 { 0 } else { self.start_before(at as usize, row) };
        Some((row, TextSize::new(start)))
    }

    /// The newlines at offsets below `at`, for `at` up to the text's length.
    fn rank(&self, at: usize) -> u32 {
        let word = at / 64;
        let block = word / BLOCK;
        let mut count = self.ranks[block];
        for bits in &self.bits[block * BLOCK..word] {
            count += bits.count_ones();
        }
        if let Some(bits) = self.bits.get(word) {
            count += (bits & below(at)).count_ones();
        }
        count
    }

    /// Just past the last newline below `at`, which has `line` (at least 1) newlines below it:
    /// the words of `at`'s block scanned down, then a select when the block has none.
    fn start_before(&self, at: usize, line: u32) -> u32 {
        let word = at / 64;
        if let Some(bits) = self.bits.get(word) {
            let found = bits & below(at);
            if found != 0 {
                return (word * 64 + 64 - found.leading_zeros() as usize) as u32;
            }
        }
        let block = word / BLOCK * BLOCK;
        for at in (block..word).rev() {
            let bits = self.bits[at];
            if bits != 0 {
                return (at * 64 + 64 - bits.leading_zeros() as usize) as u32;
            }
        }
        self.select(line - 1) + 1
    }

    /// The offset of newline `nth` (from 0), for `nth` below [`LineIndex::newlines`].
    fn select(&self, nth: u32) -> u32 {
        // The last block with no more than `nth` newlines before it holds newline `nth`; the
        // first entry is 0, so there is one.
        let block = self.ranks.partition_point(|&before| before <= nth) - 1;
        let mut left = nth - self.ranks[block];
        for (at, &bits) in self.bits.iter().enumerate().skip(block * BLOCK) {
            let ones = bits.count_ones();
            if left < ones {
                return (at * 64) as u32 + select_in_word(bits, left);
            }
            left -= ones;
        }
        unreachable!("newline {nth} of {} is in the bitmap", self.newlines())
    }
}

/// The place of set bit `nth` (from 0) of `bits`, which has more than `nth` set.
fn select_in_word(mut bits: u64, nth: u32) -> u32 {
    for _ in 0..nth {
        bits &= bits - 1;
    }
    bits.trailing_zeros()
}

/// Where an offset is in the text a [`LineIndex`] was built from: its line, and the offset its
/// line's columns count from (past the byte order mark on the first line). Its columns are
/// asked of that text; with another, they are whatever its bytes there give, or `None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    /// From 1, or 0 in a text with nothing after its byte order mark.
    line: u32,
    start: u32,
    offset: u32,
    /// The place is the text's end, just past the `\n` that ends its line (and the text).
    past_newline: bool,
}

const _: () = assert!(size_of::<Place>() == 16);

impl Place {
    /// The line as rustc numbers it, from 1: `line!()`'s, a diagnostic's, a caller
    /// location's, a line table's. 0 in a text with nothing after its byte order mark, where
    /// rustc's `lookup_line` finds none.
    pub fn line(self) -> u32 {
        self.line
    }

    /// Where the line's columns count from.
    pub fn line_start(self) -> TextSize {
        TextSize::new(self.start)
    }

    pub fn offset(self) -> TextSize {
        TextSize::new(self.offset)
    }

    /// What is written on the line before the place as rustc's text has it, without the line's
    /// `\n`, and whether the place is past that `\n` (the end of a text that ends in one). The
    /// `\r` of a `\r\n` whose `\n` is at the place, or is the one it is past, is left out.
    /// `None` when the place is not on a char boundary of `text`.
    pub fn written(self, text: &str) -> Option<(&str, bool)> {
        let mut written = text.get(self.start as usize..self.offset as usize)?;
        if self.past_newline {
            written = written.strip_suffix('\n')?;
        }
        if self.past_newline || text.as_bytes().get(self.offset as usize) == Some(&b'\n') {
            written = written.strip_suffix('\r').unwrap_or(written);
        }
        Some((written, self.past_newline))
    }

    /// The column from 1 in chars: `column!()`, a diagnostic's `file:line:col`,
    /// `proc_macro::Span::column`. Past a last `\n`, the `\n` is a char of the line.
    pub fn char_column(self, text: &str) -> Option<u32> {
        let (written, newline) = self.written(text)?;
        Some(written.chars().count() as u32 + u32::from(newline) + 1)
    }

    /// The column from 1 in display width: `Location::caller().column()`, rustc's
    /// `col_display` ([`char_width`] per char of the line's text, which has no `\n`).
    pub fn display_column(self, text: &str) -> Option<u32> {
        Some(display_width(self.written(text)?.0) + 1)
    }

    /// The column from 1 in bytes: what rustc's LLVM back end puts in DWARF's line table.
    pub fn byte_column(self, text: &str) -> Option<u32> {
        let (written, newline) = self.written(text)?;
        Some(written.len() as u32 + u32::from(newline) + 1)
    }
}

/// The columns rustc counts `written` (no newline in it) as: the sum of [`char_width`]. Text
/// that is all ASCII is a length and a count of tabs, which vectorize.
pub fn display_width(written: &str) -> u32 {
    if written.is_ascii() {
        let tabs = written.bytes().filter(|&byte| byte == b'\t').count();
        return (written.len() + 3 * tabs) as u32;
    }
    written.chars().map(char_width).sum::<usize>() as u32
}

/// How many columns rustc counts a char of a source line as (`rustc_span::char_width`): four
/// for a tab, one for each control char rustc prints a visible char for, and the display width
/// of the rest (one where it has none).
pub fn char_width(ch: char) -> usize {
    match ch {
        '\t' => 4,
        '\0'..='\u{8}'
        | '\u{b}'..='\u{1f}'
        | '\u{7f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}' => 1,
        _ => UnicodeWidthChar::width(ch).unwrap_or(1),
    }
}
