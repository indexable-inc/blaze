//! The source map: every file parsed in a session, laid out in one address space as
//! `compiler/rustc_span/src/source_map.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0) lays
//! them out, and the lookups rustfmt makes from a position or span to a file, line or text.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::{BytePos, CharPos, FileName, Pos, Span};

pub struct SourceFile {
    pub name: FileName,
    /// The text, with a UTF-8 BOM removed and `\r\n` made `\n`, as rustc keeps it.
    pub src: Option<Arc<String>>,
    pub start_pos: BytePos,
    pub source_len: u32,
    /// The start of each line, relative to `start_pos`.
    lines: Vec<u32>,
}

impl std::fmt::Debug for SourceFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SourceFile({})", self.name)
    }
}

impl SourceFile {
    fn new(name: FileName, mut src: String, start_pos: BytePos) -> SourceFile {
        normalize_src(&mut src);
        let mut lines = vec![0];
        lines.extend(
            src.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i as u32 + 1),
        );
        SourceFile {
            name,
            source_len: src.len() as u32,
            src: Some(Arc::new(src)),
            start_pos,
            lines,
        }
    }

    pub fn end_position(&self) -> BytePos {
        self.start_pos + BytePos(self.source_len)
    }

    pub fn contains(&self, pos: BytePos) -> bool {
        self.start_pos <= pos && pos <= self.end_position()
    }

    pub fn count_lines(&self) -> usize {
        self.lines.len()
    }

    /// The 0-based line `pos` is on, if it is in this file.
    pub fn lookup_line(&self, pos: BytePos) -> Option<usize> {
        let rel = pos.0.checked_sub(self.start_pos.0)?;
        Some(self.lines.partition_point(|&start| start <= rel) - 1)
    }

    pub fn line_bounds(&self, line_index: usize) -> std::ops::Range<BytePos> {
        let start = self.lines[line_index];
        let end = self
            .lines
            .get(line_index + 1)
            .copied()
            .unwrap_or(self.source_len);
        (self.start_pos + BytePos(start))..(self.start_pos + BytePos(end))
    }

    /// The text of line `line_number` (0-based), without its newline.
    pub fn get_line(&self, line_number: usize) -> Option<std::borrow::Cow<'_, str>> {
        let src = self.src.as_ref()?;
        let start = *self.lines.get(line_number)? as usize;
        let rest = &src[start..];
        let end = rest.find('\n').unwrap_or(rest.len());
        Some(std::borrow::Cow::Borrowed(&rest[..end]))
    }

    fn char_col(&self, pos: BytePos, line: usize) -> CharPos {
        let src = self.src.as_ref().unwrap();
        let start = self.lines[line] as usize;
        let rel = (pos.0 - self.start_pos.0) as usize;
        CharPos(src[start..rel.min(src.len())].chars().count())
    }
}

/// rustc's `normalize_src`: drops a UTF-8 BOM and makes each `\r\n` a `\n`, leaving a lone `\r`.
fn normalize_src(src: &mut String) {
    if src.starts_with('\u{feff}') {
        src.drain(..3);
    }
    if src.contains("\r\n") {
        *src = src.replace("\r\n", "\n");
    }
}

#[derive(Debug, Clone)]
pub struct Loc {
    pub file: Arc<SourceFile>,
    /// 1-based.
    pub line: usize,
    /// 0-based, in chars.
    pub col: CharPos,
}

#[derive(Debug)]
pub struct SourceFileAndLine {
    pub sf: Arc<SourceFile>,
    /// 0-based.
    pub line: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct LineInfo {
    /// 0-based.
    pub line_index: usize,
    pub start_col: CharPos,
    pub end_col: CharPos,
}

pub struct FileLines {
    pub file: Arc<SourceFile>,
    pub lines: Vec<LineInfo>,
}

#[derive(Debug)]
pub enum SpanSnippetError {
    DistinctSources,
    MalformedForSourcemap,
    SourceNotAvailable,
}

#[derive(Default)]
pub struct SourceMap {
    files: RefCell<Vec<Arc<SourceFile>>>,
    used_address_space: std::cell::Cell<u32>,
}

impl SourceMap {
    pub fn new() -> SourceMap {
        SourceMap::default()
    }

    pub fn files(&self) -> std::cell::Ref<'_, Vec<Arc<SourceFile>>> {
        self.files.borrow()
    }

    pub fn load_file(&self, path: &Path) -> io::Result<Arc<SourceFile>> {
        let src = fs::read_to_string(path)?;
        let name = FileName::Real(super::RealFileName::from_path(path));
        Ok(self.new_source_file(name, src))
    }

    /// Adds a file, or gives back the one of that name already added, as rustc does.
    pub fn new_source_file(&self, name: FileName, src: String) -> Arc<SourceFile> {
        if let Some(file) = self.get_source_file(&name) {
            return file;
        }
        let size = src.len() as u32;
        let start_pos = BytePos(self.used_address_space.get());
        self.used_address_space.set(start_pos.0 + size + 1);
        let file = Arc::new(SourceFile::new(name, src, start_pos));
        self.files.borrow_mut().push(file.clone());
        file
    }

    pub fn get_source_file(&self, name: &FileName) -> Option<Arc<SourceFile>> {
        self.files
            .borrow()
            .iter()
            .find(|f| f.name == *name)
            .cloned()
    }

    pub fn lookup_source_file(&self, pos: BytePos) -> Arc<SourceFile> {
        let files = self.files.borrow();
        let i = files
            .partition_point(|f| f.start_pos <= pos)
            .saturating_sub(1);
        files[i].clone()
    }

    pub fn lookup_char_pos(&self, pos: BytePos) -> Loc {
        let file = self.lookup_source_file(pos);
        let line = file.lookup_line(pos).unwrap_or(0);
        let col = file.char_col(pos, line);
        Loc {
            file,
            line: line + 1,
            col,
        }
    }

    pub fn lookup_line(&self, pos: BytePos) -> Result<SourceFileAndLine, Arc<SourceFile>> {
        let sf = self.lookup_source_file(pos);
        match sf.lookup_line(pos) {
            Some(line) => Ok(SourceFileAndLine { sf, line }),
            None => Err(sf),
        }
    }

    pub fn span_to_filename(&self, span: Span) -> FileName {
        self.lookup_source_file(span.lo()).name.clone()
    }

    pub fn span_to_snippet(&self, span: Span) -> Result<String, SpanSnippetError> {
        let file = self.lookup_source_file(span.lo());
        if !file.contains(span.hi()) {
            return Err(SpanSnippetError::DistinctSources);
        }
        let src = file
            .src
            .as_ref()
            .ok_or(SpanSnippetError::SourceNotAvailable)?;
        let lo = (span.lo().0 - file.start_pos.0) as usize;
        let hi = (span.hi().0 - file.start_pos.0) as usize;
        src.get(lo..hi)
            .map(str::to_owned)
            .ok_or(SpanSnippetError::MalformedForSourcemap)
    }

    pub fn span_to_lines(&self, span: Span) -> Result<FileLines, SpanSnippetError> {
        let lo = self.lookup_char_pos(span.lo());
        let hi = self.lookup_char_pos(span.hi());
        if !Arc::ptr_eq(&lo.file, &hi.file) {
            return Err(SpanSnippetError::DistinctSources);
        }
        let mut lines = Vec::new();
        let mut start_col = lo.col;
        for line_index in lo.line - 1..hi.line - 1 {
            let line_len = lo
                .file
                .get_line(line_index)
                .map_or(0, |s| s.chars().count());
            lines.push(LineInfo {
                line_index,
                start_col,
                end_col: CharPos::from_usize(line_len),
            });
            start_col = CharPos(0);
        }
        lines.push(LineInfo {
            line_index: hi.line - 1,
            start_col,
            end_col: hi.col,
        });
        Ok(FileLines {
            file: lo.file,
            lines,
        })
    }

    /// `file:line:col: line:col`, as rustc writes a span in a diagnostic's text.
    pub fn span_to_diagnostic_string(&self, span: Span) -> String {
        let lo = self.lookup_char_pos(span.lo());
        let hi = self.lookup_char_pos(span.hi());
        format!(
            "{}:{}:{}: {}:{}",
            lo.file.name,
            lo.line,
            lo.col.to_usize() + 1,
            hi.line,
            hi.col.to_usize() + 1
        )
    }
}
