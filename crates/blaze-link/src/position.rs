//! Positions the program observes, filled at link time (docs/v2/plan/lanes/link/notes.md, "Positions").
//!
//! Code never holds a position: `line!()`, `file!()`, `column!()`, a panic's and a
//! `#[track_caller]` function's `Location` are a [`blaze_codegen::Target::Position`]
//! relocation to a `core::panic::Location` record the linker makes from the span side table
//! of the span's anchor. A reformat changes the table, so the record is rewritten in place and
//! no function is compiled again.
//!
//! The image keeps each table as [`Spans`]: its encoding, checked once and read in place (one
//! allocation per table, none per file or span). A table the change carries again unchanged
//! is told by comparing bytes, before anything is decoded.

use std::sync::Arc;

use blaze_format::cursor::{Put, Reader};
use blaze_ids::ContentId;

/// The span side table of one anchor, as [`crate::Change::spans`] carries it.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct SpanTable {
    /// The file names the spans are in, as the program observes them (`file!()`).
    pub files: Vec<String>,
    /// By the span's index: the file (an index into `files`), its 1-based line and column.
    pub spans: Vec<Position>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Position {
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

const MAGIC: [u8; 8] = *b"BZSPANS1";
/// A span's three `u32`s.
const SPAN: usize = 12;

impl SpanTable {
    /// `MAGIC`, the file count and each file as a length and its UTF-8, the span count and
    /// each span as three little-endian `u32`s.
    pub fn encode(&self) -> Vec<u8> {
        let names: usize = self.files.iter().map(|it| 4 + it.len()).sum();
        let mut out = Vec::with_capacity(8 + 4 + names + 4 + SPAN * self.spans.len());
        out.put_bytes(&MAGIC);
        out.put_u32(self.files.len() as u32);
        for file in &self.files {
            out.put_bytes_u32(file.as_bytes());
        }
        out.put_u32(self.spans.len() as u32);
        for span in &self.spans {
            for word in [span.file, span.line, span.column] {
                out.put_u32(word);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<SpanTable, String> {
        let spans = Spans::new(bytes)?;
        let mut files = Vec::with_capacity(spans.files() as usize);
        // alloc: the edge: the public `SpanTable` owns its file names
        files.extend((0..spans.files()).map(|it| spans.file(it).to_owned()));
        let mut list = Vec::with_capacity(spans.len() as usize);
        list.extend((0..spans.len()).map(|it| spans.span(it).expect("in range")));
        Ok(SpanTable { files, spans: list })
    }

    /// The content ID of the table, which the image keeps to tell a changed one.
    pub fn id(&self) -> ContentId {
        ContentId::of(SPAN_TABLE_DOMAIN, &self.encode())
    }
}

/// A checked span table, read in place. One buffer: the file names' starts (a `u32` each,
/// pointing at the name's bytes in the encoding), then the encoding itself. Cloning is an
/// `Arc` count; reading a span is three loads, a file name two.
#[derive(Clone, Debug)]
pub(crate) struct Spans {
    buf: Arc<[u8]>,
    files: u32,
    /// Where the spans start in `buf`, and how many there are.
    spans_at: u32,
    spans: u32,
}

impl Spans {
    /// Checks `bytes` as a span table and keeps one copy of it.
    pub fn new(bytes: &[u8]) -> Result<Spans, String> {
        let mut read = Reader::new(bytes);
        if read.take(8)? != MAGIC {
            // alloc: error path
            return Err("not a span table: its magic is wrong".to_owned());
        }
        // Each file takes at least its length's 4 bytes, so the index is no longer than the table.
        let files = read.count_u32(4)?;
        let index = files * 4;
        // An exact-size iterator collects into the `Arc` itself.
        // alloc: the table's one buffer, once per table
        let mut buf: Arc<[u8]> = std::iter::repeat_n(0, index + bytes.len()).collect();
        let out = Arc::get_mut(&mut buf).expect("just made");
        for file in 0..files {
            let start = read.at() + 4;
            read.str_u32()?;
            out[file * 4..file * 4 + 4].copy_from_slice(&((index + start) as u32).to_le_bytes());
        }
        let spans = read.count_u32(SPAN)?;
        let spans_at = read.at();
        let rest = read.take(spans * SPAN)?;
        if !read.is_done() {
            // alloc: error path
            return Err("bytes after a span table".to_owned());
        }
        for span in rest.chunks_exact(SPAN) {
            let file = u32::from_le_bytes(span[..4].try_into().expect("four bytes"));
            if file as usize >= files {
                // alloc: error path
                return Err(format!("a span in file {file} of {files}"));
            }
        }
        out[index..].copy_from_slice(bytes);
        let spans_at = u32::try_from(index + spans_at).map_err(|_| "a span table past 4 GiB")?;
        Ok(Spans { buf, files: files as u32, spans_at, spans: spans as u32 })
    }

    /// The table's encoding, as the change carried it.
    pub fn encoding(&self) -> &[u8] {
        &self.buf[self.files as usize * 4..]
    }

    /// Whether `bytes` is this table: a comparison, nothing decoded.
    pub fn is(&self, bytes: &[u8]) -> bool {
        self.encoding() == bytes
    }

    pub fn files(&self) -> u32 {
        self.files
    }

    pub fn len(&self) -> u32 {
        self.spans
    }

    /// The name of file `file` (below [`Spans::files`]).
    pub fn file(&self, file: u32) -> &str {
        let at = file as usize * 4;
        let start = u32::from_le_bytes(self.buf[at..at + 4].try_into().expect("four bytes")) as usize;
        let len = u32::from_le_bytes(self.buf[start - 4..start].try_into().expect("four bytes")) as usize;
        std::str::from_utf8(&self.buf[start..start + len]).expect("checked UTF-8 when made")
    }

    pub fn span(&self, index: u32) -> Option<Position> {
        if index >= self.spans {
            return None;
        }
        let at = self.spans_at as usize + index as usize * SPAN;
        let word = |n: usize| u32::from_le_bytes(self.buf[at + n * 4..at + n * 4 + 4].try_into().expect("four bytes"));
        Some(Position { file: word(0), line: word(1), column: word(2) })
    }

    pub fn id(&self) -> ContentId {
        ContentId::of(SPAN_TABLE_DOMAIN, self.encoding())
    }
}

pub(crate) const SPAN_TABLE_DOMAIN: &str = "blaze v2 span table 1";
pub(crate) const FILE_NAME_DOMAIN: &str = "blaze v2 link file name 1";

/// `core::panic::Location<'static>` as blaze lays it out (rustc's field order by alignment):
/// the file name's pointer and length (`NonNull<str>`), then `line` and `col` as `u32`s.
/// codegen reads `line!()` at [`LINE`], `column!()` at [`COLUMN`] and `file!()`'s `&str` at 0.
pub const LOCATION_SIZE: u64 = 24;
pub const LOCATION_ALIGN: u64 = 8;
pub const LINE: i64 = 16;
pub const COLUMN: i64 = 20;

/// A `Location` record's bytes, its pointer left for a relocation to the file name.
pub fn location(name_len: usize, position: Position) -> [u8; LOCATION_SIZE as usize] {
    let mut record = [0; LOCATION_SIZE as usize];
    record[8..16].copy_from_slice(&(name_len as u64).to_le_bytes());
    record[16..20].copy_from_slice(&position.line.to_le_bytes());
    record[20..24].copy_from_slice(&position.column.to_le_bytes());
    record
}

/// The bytes of a file name a `Location` points at: NUL-terminated, as
/// `Location::file_as_c_str` reads one byte past the length.
pub fn file_name(name: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(name.len() + 1);
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> SpanTable {
        SpanTable {
            files: vec!["src/main.rs".to_owned(), "src/lib/ünï.rs".to_owned()],
            spans: vec![
                Position { file: 0, line: 3, column: 5 },
                Position { file: 1, line: 120, column: 1 },
            ],
        }
    }

    #[test]
    fn a_table_round_trips() {
        let bytes = table().encode();
        assert_eq!(SpanTable::decode(&bytes).unwrap(), table());
    }

    #[test]
    fn a_bad_table_is_refused_not_misread() {
        let bytes = table().encode();
        for len in 0..bytes.len() {
            assert!(SpanTable::decode(&bytes[..len]).is_err(), "cut at {len}");
            assert!(Spans::new(&bytes[..len]).is_err(), "cut at {len}");
        }
        let mut wrong_file = table();
        wrong_file.spans[0].file = 2;
        assert!(SpanTable::decode(&wrong_file.encode()).is_err());
        let mut more = bytes.clone();
        more.push(0);
        assert!(SpanTable::decode(&more).is_err());
        let mut not_utf8 = bytes.clone();
        not_utf8[16] = 0xff;
        assert!(Spans::new(&not_utf8).is_err());
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Spans::new(&huge).is_err());
    }

    #[test]
    fn spans_read_in_place_what_the_owned_table_holds() {
        let owned = table();
        let bytes = owned.encode();
        let spans = Spans::new(&bytes).unwrap();
        assert!(spans.is(&bytes));
        assert_eq!(spans.encoding(), &bytes[..]);
        assert_eq!(spans.id(), owned.id());
        assert_eq!((spans.files(), spans.len()), (2, 2));
        for (at, name) in owned.files.iter().enumerate() {
            assert_eq!(spans.file(at as u32), name);
        }
        for (at, span) in owned.spans.iter().enumerate() {
            assert_eq!(spans.span(at as u32), Some(*span));
        }
        assert_eq!(spans.span(2), None);
        let mut other = table();
        other.spans[1].line = 121;
        assert!(!spans.is(&other.encode()));
        let empty = SpanTable::default().encode();
        let none = Spans::new(&empty).unwrap();
        assert_eq!((none.files(), none.len(), none.span(0)), (0, 0, None));
    }

    /// Checking a table is one allocation whatever its size; reading a span or a file name,
    /// and telling an unchanged table, none.
    #[test]
    fn a_table_is_one_allocation_and_reads_none() {
        for n in [2u32, 5000] {
            let owned = SpanTable {
                files: (0..n).map(|it| format!("src/f{it}.rs")).collect(),
                spans: (0..n).map(|it| Position { file: it, line: it + 1, column: 1 }).collect(),
            };
            let bytes = owned.encode();
            let spans = blaze_alloc_count::assert_at_most(1, "a table checked and kept", || Spans::new(&bytes).unwrap());
            blaze_alloc_count::assert_at_most(0, "a span, a file name, a comparison", || {
                assert_eq!(spans.span(n - 1), Some(Position { file: n - 1, line: n, column: 1 }));
                assert_eq!(spans.file(n - 1), owned.files[n as usize - 1]);
                assert!(spans.is(&bytes));
            });
        }
    }

    #[test]
    fn a_location_has_lines_where_codegen_reads_them() {
        let record = location(11, Position { file: 0, line: 7, column: 9 });
        assert_eq!(u64::from_le_bytes(record[8..16].try_into().unwrap()), 11);
        assert_eq!(u32::from_le_bytes(record[LINE as usize..][..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(record[COLUMN as usize..][..4].try_into().unwrap()), 9);
        assert_eq!(file_name("a.rs"), b"a.rs\0");
    }
}
