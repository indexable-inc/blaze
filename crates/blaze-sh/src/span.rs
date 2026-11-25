//! Line-relative byte spans for tokens, AST nodes, and diagnostics (rule 71).

/// A half-open byte range `[start, end)` relative to the submitted source text.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const ZERO: Span = Span { start: 0, end: 0 };

    #[inline]
    pub const fn new(start: u32, end: u32) -> Span {
        if end < start { Span { start, end: start } } else { Span { start, end } }
    }

    #[inline]
    pub const fn from_usize(start: usize, end: usize) -> Span {
        let s = if start > u32::MAX as usize { u32::MAX } else { start as u32 };
        let e = if end > u32::MAX as usize { u32::MAX } else { end as u32 };
        Span::new(s, e)
    }

    #[inline]
    pub const fn merge(self, other: Span) -> Span {
        let start = if self.start < other.start { self.start } else { other.start };
        let end = if self.end > other.end { self.end } else { other.end };
        Span { start, end }
    }

    #[inline]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    /// Borrow the covered slice of `src`, clamped safely to UTF-8 boundaries.
    pub fn slice<'s>(self, src: &'s str) -> &'s str {
        let len = src.len();
        let mut s = (self.start as usize).min(len);
        let mut e = (self.end as usize).min(len);
        if e < s {
            e = s;
        }
        while s > 0 && !src.is_char_boundary(s) {
            s -= 1;
        }
        while e < len && !src.is_char_boundary(e) {
            e += 1;
        }
        &src[s..e]
    }
}

/// 1-based line and column computed on demand for display only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
    pub line_start: usize,
    pub line_end: usize,
}

/// Compute 1-based line/column and enclosing line byte range for `offset` inside `src`.
pub fn line_col(src: &str, offset: u32) -> LineCol {
    let target = (offset as usize).min(src.len());
    let mut line = 1u32;
    let mut line_start = 0usize;
    for (i, b) in src.bytes().enumerate() {
        if i == target {
            break;
        }
        if b == b'\n' {
            line = line.saturating_add(1);
            line_start = i + 1;
        }
    }
    let line_end = src[line_start..]
        .find('\n')
        .map(|rel| line_start + rel)
        .unwrap_or(src.len());
    let col = src[line_start..target.max(line_start).min(src.len())].chars().count() as u32 + 1;
    LineCol { line, col, line_start, line_end }
}
