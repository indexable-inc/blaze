//! A file's line table: where each line starts, so a finding's item-relative range becomes
//! `file:line:col` only when it is printed (bottom.md 12, rule 71).

use core::alloc::Allocator;

use blaze_own::Vec;

/// The byte offset of each line's first byte, ascending; line 1 starts at 0.
pub struct LineTable<A: Allocator> {
    starts: Vec<u32, A>,
}

/// A place as a person reads it: line and column counted from 1, the column in characters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

impl<A: Allocator> LineTable<A> {
    /// The table of `text`, in `alloc`. A file past `u32::MAX` bytes has no table: the outline
    /// refuses such a file first.
    pub fn new_in(text: &[u8], alloc: A) -> LineTable<A> {
        let mut starts = Vec::new_in(alloc);
        starts.push(0);
        for (at, byte) in text.iter().enumerate() {
            if *byte == b'\n' {
                starts.push(u32::try_from(at + 1).expect("a file the outline took is under 4 GiB"));
            }
        }
        LineTable { starts }
    }

    /// The position of byte `offset` of `text` (the text the table was made from). An offset
    /// past the end is placed at the end.
    pub fn position(&self, text: &[u8], offset: u32) -> Position {
        let offset = offset.min(u32::try_from(text.len()).unwrap_or(u32::MAX));
        let line = match self.starts.binary_search(&offset) {
            Ok(at) => at,
            Err(at) => at - 1,
        };
        let start = self.starts[line] as usize;
        let prefix = &text[start..offset as usize];
        // Characters, not bytes: a continuation byte of UTF-8 starts no character.
        let column = prefix.iter().filter(|byte| (**byte & 0xC0) != 0x80).count();
        Position {
            line: u32::try_from(line + 1).unwrap_or(u32::MAX),
            column: u32::try_from(column + 1).unwrap_or(u32::MAX),
        }
    }

    pub fn lines(&self) -> usize {
        self.starts.len()
    }
}
