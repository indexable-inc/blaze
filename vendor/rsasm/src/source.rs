//! Source file management.
//!
//! All source text lives in a [`SourceMap`]. Every byte of every file is
//! addressable by a single global `u32` position, which lets [`Span`] stay
//! small (8 bytes) and lets diagnostics point anywhere without carrying a file
//! handle around.

use std::fmt;
use std::path::{Path, PathBuf};

/// Identifies one file inside a [`SourceMap`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileId(pub u32);

/// A half-open byte range `[lo, hi)` in the [`SourceMap`]'s global position
/// space.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Span {
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub const DUMMY: Span = Span { lo: 0, hi: 0 };

    pub fn new(lo: u32, hi: u32) -> Span {
        debug_assert!(lo <= hi);
        Span { lo, hi }
    }

    /// The smallest span covering both inputs.
    pub fn to(self, other: Span) -> Span {
        if self == Span::DUMMY {
            return other;
        }
        if other == Span::DUMMY {
            return self;
        }
        Span::new(self.lo.min(other.lo), self.hi.max(other.hi))
    }

    /// A zero-width span at the start of this one.
    pub fn shrink_to_lo(self) -> Span {
        Span::new(self.lo, self.lo)
    }

    pub fn is_dummy(self) -> bool {
        self == Span::DUMMY
    }

    pub fn len(self) -> u32 {
        self.hi - self.lo
    }

    pub fn is_empty(self) -> bool {
        self.hi == self.lo
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.lo, self.hi)
    }
}

/// A 1-based line and column, ready for display.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct LineCol {
    pub line: u32,
    /// 1-based column counted in characters, not bytes.
    pub col: u32,
}

#[non_exhaustive]
pub struct SourceFile {
    pub id: FileId,
    pub name: PathBuf,
    pub src: String,
    /// Global position of byte 0 of this file.
    pub start: u32,
    /// Global position of the first byte of each line.
    line_starts: Vec<u32>,
}

impl SourceFile {
    fn new(id: FileId, name: PathBuf, src: String, start: u32) -> SourceFile {
        let mut line_starts = vec![start];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(start + i as u32 + 1);
            }
        }
        SourceFile {
            id,
            name,
            src,
            start,
            line_starts,
        }
    }

    pub fn end(&self) -> u32 {
        self.start + self.src.len() as u32
    }

    fn contains(&self, pos: u32) -> bool {
        pos >= self.start && pos <= self.end()
    }

    /// 0-based line index containing `pos`.
    fn line_index(&self, pos: u32) -> usize {
        match self.line_starts.binary_search(&pos) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    pub fn line_col(&self, pos: u32) -> LineCol {
        let idx = self.line_index(pos);
        let line_start = self.line_starts[idx];
        let text = &self.src[(line_start - self.start) as usize..(pos - self.start) as usize];
        LineCol {
            line: idx as u32 + 1,
            col: text.chars().count() as u32 + 1,
        }
    }

    /// Text of the 1-based line `line`, without its trailing newline.
    pub fn line_text(&self, line: u32) -> &str {
        let idx = (line - 1) as usize;
        let Some(&lo) = self.line_starts.get(idx) else {
            return "";
        };
        let hi = self
            .line_starts
            .get(idx + 1)
            .copied()
            .unwrap_or_else(|| self.end());
        let s = &self.src[(lo - self.start) as usize..(hi - self.start) as usize];
        let s = s.strip_suffix('\n').unwrap_or(s);
        s.strip_suffix('\r').unwrap_or(s)
    }

    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }
}

/// Owns every source file loaded during a run.
#[derive(Default)]
#[non_exhaustive]
pub struct SourceMap {
    files: Vec<SourceFile>,
    next_start: u32,
}

impl SourceMap {
    pub fn new() -> SourceMap {
        // Position 0 is reserved so that `Span::DUMMY` never aliases real text.
        SourceMap {
            files: Vec::new(),
            next_start: 1,
        }
    }

    /// Registers already-loaded text under a display name.
    pub fn add(&mut self, name: impl Into<PathBuf>, src: impl Into<String>) -> FileId {
        let id = FileId(self.files.len() as u32);
        let src = src.into();
        let start = self.next_start;
        // +1 so adjacent files never share a boundary position.
        self.next_start = start + src.len() as u32 + 1;
        self.files
            .push(SourceFile::new(id, name.into(), src, start));
        id
    }

    pub fn load(&mut self, path: &Path) -> std::io::Result<FileId> {
        let src = std::fs::read_to_string(path)?;
        Ok(self.add(path.to_path_buf(), src))
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }

    /// The file containing `pos`, if any.
    pub fn lookup(&self, pos: u32) -> Option<&SourceFile> {
        let idx = match self.files.binary_search_by_key(&pos, |f| f.start) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let f = &self.files[idx];
        f.contains(pos).then_some(f)
    }

    /// The global position of the start of the line containing `pos`.
    pub fn line_start_of(&self, pos: u32) -> u32 {
        match self.lookup(pos) {
            Some(f) => f.line_starts[f.line_index(pos)],
            None => pos,
        }
    }

    /// The text covered by `span`. Empty if the span is dummy or malformed.
    pub fn span_text(&self, span: Span) -> &str {
        let Some(f) = self.lookup(span.lo) else {
            return "";
        };
        let lo = (span.lo - f.start) as usize;
        let hi = ((span.hi - f.start) as usize).min(f.src.len());
        if lo > hi {
            return "";
        }
        &f.src[lo..hi]
    }
}
