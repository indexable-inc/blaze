//! Positions the program observes, filled at link time (docs/v2/plan.md, "Positions").
//!
//! Code never holds a position: `line!()`, `file!()`, `column!()`, a panic's and a
//! `#[track_caller]` function's `Location` are a [`blaze_codegen::Target::Position`]
//! relocation to a `core::panic::Location` record the linker makes from the span side table
//! of the span's anchor. A reformat changes the table, so the record is rewritten in place and
//! no function is compiled again.
//!
//! A table is item-relative: each span is a row counted from one of the table's few bases
//! (where its item's anchor begins, where its owner is declared), or, for a span in another
//! file (a macro written elsewhere), absolute. A line added above an item moves its bases and
//! nothing else, so its table's encoding is the one before but for its last section, the
//! bases ([`SpanTable::encode`]). The `Location` record is absolute, as Rust lays it out:
//! the link makes its line from base plus row ([`Spans::span`]), here and nowhere else.
//!
//! The image keeps each table as [`Spans`]: its encoding, checked once and read in place (one
//! allocation per table, none per file or span). A table the change carries again unchanged
//! is told by comparing bytes, before anything is decoded.

use std::sync::Arc;

use blaze_format::cursor::{Put, Reader};
use blaze_ids::ContentId;

/// The span side table of one anchor, as [`crate::Change::spans`] carries it.
///
/// The invariant: every [`Span::From`] names a base of `bases` and resolves to a line and a
/// column that fit a `u32`, and every file index is below `files.len()`; a table that breaks
/// it is refused when read ([`Spans::new`]), never misread.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct SpanTable {
    /// The file names the spans are in, as the program observes them (`file!()`).
    pub files: Vec<String>,
    /// By the span's index: where it is, item-relative or absolute.
    pub spans: Vec<Span>,
    /// Where the table's relative spans are counted from, absolute: the one part of the table
    /// a line added above its item changes.
    pub bases: Vec<Position>,
}

/// A position as the program observes it: the file (an index into the table's files), its
/// 1-based line and column.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Position {
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

/// One span of a [`SpanTable`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Span {
    /// Where it is, absolute: a span of another file than its item's.
    At(Position),
    /// `line` lines below base `base`, in its file. On the base's own line (`line == 0`)
    /// `column` columns right of the base's; on a later one, the 1-based column itself.
    From { base: u32, line: u32, column: u32 },
}

impl From<Position> for Span {
    fn from(position: Position) -> Span {
        Span::At(position)
    }
}

impl Span {
    /// Where the span is, resolved against `bases`: `None` for a base `bases` lacks or a line
    /// or column past `u32`.
    pub fn resolve(self, bases: &[Position]) -> Option<Position> {
        match self {
            Span::At(position) => Some(position),
            Span::From { base, line, column } => {
                resolve(*bases.get(base as usize)?, line, column)
            }
        }
    }
}

/// [`Span::From`] made absolute from `base`: the one rule.
fn resolve(base: Position, line: u32, column: u32) -> Option<Position> {
    let column = if line == 0 { base.column.checked_add(column)? } else { column };
    Some(Position { file: base.file, line: base.line.checked_add(line)?, column })
}

const MAGIC: [u8; 8] = *b"BZSPANS2";
/// A span's four `u32`s: the base ([`ABSOLUTE`] for none), the file, the line and the column.
const SPAN: usize = 16;
/// A base's three `u32`s.
const BASE: usize = 12;
/// The base word of a [`Span::At`].
const ABSOLUTE: u32 = u32::MAX;

impl SpanTable {
    /// `MAGIC`, the file count and each file as a length and its UTF-8, the span count and
    /// each span as four little-endian `u32`s (base, file, line, column; a relative span's
    /// file is 0, its base's file being its own), then the base count and each base as three.
    /// The bases come last, so a table whose item moved is the same bytes up to them.
    pub fn encode(&self) -> Vec<u8> {
        let names: usize = self.files.iter().map(|it| 4 + it.len()).sum();
        let mut out = Vec::with_capacity(
            8 + 4 + names + 4 + SPAN * self.spans.len() + 4 + BASE * self.bases.len(),
        );
        out.put_bytes(&MAGIC);
        out.put_u32(self.files.len() as u32);
        for file in &self.files {
            out.put_bytes_u32(file.as_bytes());
        }
        out.put_u32(self.spans.len() as u32);
        for span in &self.spans {
            let words = match *span {
                Span::At(it) => [ABSOLUTE, it.file, it.line, it.column],
                Span::From { base, line, column } => [base, 0, line, column],
            };
            for word in words {
                out.put_u32(word);
            }
        }
        out.put_u32(self.bases.len() as u32);
        for base in &self.bases {
            for word in [base.file, base.line, base.column] {
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
        list.extend((0..spans.len()).map(|it| spans.row(it)));
        let mut bases = Vec::with_capacity(spans.bases as usize);
        bases.extend((0..spans.bases).map(|it| spans.base(it)));
        Ok(SpanTable { files, spans: list, bases })
    }

    /// The content ID of the table, which the image keeps to tell a changed one.
    pub fn id(&self) -> ContentId {
        ContentId::of(SPAN_TABLE_DOMAIN, &self.encode())
    }
}

/// The file name and the absolute position of span `index` of `table`, a table as
/// [`SpanTable::encode`] writes it, read in place: for a reader with no image to keep the
/// table in (the launch record, `blaze_launch_host::record`). It reads the file names'
/// lengths, the one span and its base, no more, and allocates only an error.
pub fn position_in(table: &[u8], index: u32) -> Result<(&str, Position), String> {
    let mut read = Reader::new(table);
    if read.take(8)? != MAGIC {
        // alloc: an error
        return Err("its span table does not read: its magic is wrong".to_owned());
    }
    let files = read.count_u32(4)?;
    let names = read.at();
    for _ in 0..files {
        read.bytes_u32()?;
    }
    let spans = read.count_u32(SPAN)?;
    if index as usize >= spans {
        // alloc: an error
        return Err(format!("its span table has {spans} spans"));
    }
    let rows = read.at();
    read.skip(spans * SPAN)?;
    let bases = read.count_u32(BASE)?;
    let bases_at = read.at();
    let word = |at: usize| -> Result<u32, String> {
        Ok(Reader::starting_at(table, at)?.u32()?)
    };
    let row = rows + index as usize * SPAN;
    let (from, file, line, column) = (word(row)?, word(row + 4)?, word(row + 8)?, word(row + 12)?);
    let position = match from {
        ABSOLUTE => Position { file, line, column },
        from if (from as usize) < bases && file == 0 => {
            let at = bases_at + from as usize * BASE;
            let base = Position { file: word(at)?, line: word(at + 4)?, column: word(at + 8)? };
            // alloc: an error
            resolve(base, line, column)
                .ok_or_else(|| format!("span {index} is past the last line a `u32` holds"))?
        }
        // alloc: an error
        from => return Err(format!("span {index} counted from base {from} of {bases}")),
    };
    if position.file as usize >= files {
        // alloc: an error
        return Err(format!("span {index} in file {} of {files}", position.file));
    }
    let mut read = Reader::starting_at(table, names)?;
    for _ in 0..position.file {
        read.bytes_u32()?;
    }
    Ok((read.str_u32()?, position))
}

/// A checked span table, read in place. One buffer: the file names' starts (a `u32` each,
/// pointing at the name's bytes in the encoding), then the encoding itself. Cloning is an
/// `Arc` count; reading a span is at most seven loads, a file name two.
#[derive(Clone, Debug)]
pub(crate) struct Spans {
    buf: Arc<[u8]>,
    files: u32,
    /// Where the spans start in `buf`, and how many there are.
    spans_at: u32,
    spans: u32,
    /// Where the bases start in `buf`, and how many there are.
    bases_at: u32,
    bases: u32,
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
        let rows = read.take(spans * SPAN)?;
        let bases = read.count_u32(BASE)?;
        let bases_at = read.at();
        let based = read.take(bases * BASE)?;
        if !read.is_done() {
            // alloc: error path
            return Err("bytes after a span table".to_owned());
        }
        let word = |chunk: &[u8], n: usize| {
            u32::from_le_bytes(chunk[n * 4..n * 4 + 4].try_into().expect("four bytes"))
        };
        let base = |at: usize| {
            let chunk = &based[at * BASE..at * BASE + BASE];
            Position { file: word(chunk, 0), line: word(chunk, 1), column: word(chunk, 2) }
        };
        for at in 0..bases {
            let file = base(at).file;
            if file as usize >= files {
                // alloc: error path
                return Err(format!("base {at} in file {file} of {files}"));
            }
        }
        for (at, span) in rows.chunks_exact(SPAN).enumerate() {
            let (from, file) = (word(span, 0), word(span, 1));
            if from == ABSOLUTE {
                if file as usize >= files {
                    // alloc: error path
                    return Err(format!("span {at} in file {file} of {files}"));
                }
                continue;
            }
            if from as usize >= bases || file != 0 {
                // alloc: error path
                return Err(format!(
                    "span {at} counted from base {from} of {bases}, in file {file}"
                ));
            }
            if resolve(base(from as usize), word(span, 2), word(span, 3)).is_none() {
                // alloc: error path
                return Err(format!("span {at} is past the last line or column a `u32` holds"));
            }
        }
        out[index..].copy_from_slice(bytes);
        let at = |it: usize| u32::try_from(index + it).map_err(|_| "a span table past 4 GiB");
        Ok(Spans {
            buf,
            files: files as u32,
            spans_at: at(spans_at)?,
            spans: spans as u32,
            bases_at: at(bases_at)?,
            bases: bases as u32,
        })
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
        let start =
            u32::from_le_bytes(self.buf[at..at + 4].try_into().expect("four bytes")) as usize;
        let len =
            u32::from_le_bytes(self.buf[start - 4..start].try_into().expect("four bytes")) as usize;
        std::str::from_utf8(&self.buf[start..start + len]).expect("checked UTF-8 when made")
    }

    fn word(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.buf[at..at + 4].try_into().expect("four bytes"))
    }

    /// Base `index` (below the table's base count).
    fn base(&self, index: u32) -> Position {
        let at = self.bases_at as usize + index as usize * BASE;
        Position { file: self.word(at), line: self.word(at + 4), column: self.word(at + 8) }
    }

    /// Span `index` (below [`Spans::len`]) as the table holds it.
    fn row(&self, index: u32) -> Span {
        let at = self.spans_at as usize + index as usize * SPAN;
        let (base, file, line, column) =
            (self.word(at), self.word(at + 4), self.word(at + 8), self.word(at + 12));
        match base {
            ABSOLUTE => Span::At(Position { file, line, column }),
            base => Span::From { base, line, column },
        }
    }

    /// Where span `index` is, absolute: as the `Location` record of it holds it.
    pub fn span(&self, index: u32) -> Option<Position> {
        if index >= self.spans {
            return None;
        }
        match self.row(index) {
            Span::At(it) => Some(it),
            Span::From { base, line, column } => {
                Some(resolve(self.base(base), line, column).expect("checked when made"))
            }
        }
    }

    pub fn id(&self) -> ContentId {
        ContentId::of(SPAN_TABLE_DOMAIN, self.encoding())
    }
}

pub(crate) const SPAN_TABLE_DOMAIN: &str = "blaze v2 span table 2";
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

/// `core::panic::Location<'static>` on wasm32, where a pointer is four bytes: the file name's
/// pointer and length, then `line` and `col`, each four bytes.
pub const WASM32_LOCATION_SIZE: u64 = 16;
pub const WASM32_LOCATION_ALIGN: u64 = 4;

/// A wasm32 `Location` record's bytes, its pointer left for a relocation to the file name.
pub fn wasm32_location(name_len: usize, position: Position) -> [u8; WASM32_LOCATION_SIZE as usize] {
    let mut record = [0; WASM32_LOCATION_SIZE as usize];
    record[4..8].copy_from_slice(&(name_len as u32).to_le_bytes());
    record[8..12].copy_from_slice(&position.line.to_le_bytes());
    record[12..16].copy_from_slice(&position.column.to_le_bytes());
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
                Span::At(Position { file: 0, line: 3, column: 5 }),
                Span::At(Position { file: 1, line: 120, column: 1 }),
                Span::From { base: 0, line: 0, column: 4 },
                Span::From { base: 0, line: 2, column: 9 },
                Span::From { base: 1, line: 0, column: 0 },
            ],
            bases: vec![
                Position { file: 1, line: 40, column: 5 },
                Position { file: 0, line: 7, column: 1 },
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
        wrong_file.spans[0] = Span::At(Position { file: 2, line: 1, column: 1 });
        assert!(SpanTable::decode(&wrong_file.encode()).is_err());
        let mut wrong_base = table();
        wrong_base.spans[2] = Span::From { base: 2, line: 0, column: 0 };
        assert!(SpanTable::decode(&wrong_base.encode()).is_err());
        let mut base_file = table();
        base_file.bases[0].file = 2;
        assert!(SpanTable::decode(&base_file.encode()).is_err());
        let mut past = table();
        past.spans[3] = Span::From { base: 0, line: u32::MAX, column: 1 };
        assert!(SpanTable::decode(&past.encode()).is_err(), "a line past `u32`");
        let mut more = bytes.clone();
        more.push(0);
        assert!(SpanTable::decode(&more).is_err());
        let mut not_utf8 = bytes.clone();
        not_utf8[16] = 0xff;
        assert!(Spans::new(&not_utf8).is_err());
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Spans::new(&huge).is_err());
        let mut old = bytes.clone();
        old[..8].copy_from_slice(b"BZSPANS1");
        assert!(Spans::new(&old).is_err(), "a table of the layout before is refused");
    }

    #[test]
    fn spans_read_in_place_what_the_owned_table_holds() {
        let owned = table();
        let bytes = owned.encode();
        let spans = Spans::new(&bytes).unwrap();
        assert!(spans.is(&bytes));
        assert_eq!(spans.encoding(), &bytes[..]);
        assert_eq!(spans.id(), owned.id());
        assert_eq!((spans.files(), spans.len()), (2, 5));
        for (at, name) in owned.files.iter().enumerate() {
            assert_eq!(spans.file(at as u32), name);
        }
        for (at, span) in owned.spans.iter().enumerate() {
            assert_eq!(spans.span(at as u32), span.resolve(&owned.bases));
        }
        assert_eq!(spans.span(2), Some(Position { file: 1, line: 40, column: 9 }));
        assert_eq!(spans.span(3), Some(Position { file: 1, line: 42, column: 9 }));
        assert_eq!(spans.span(4), Some(Position { file: 0, line: 7, column: 1 }));
        assert_eq!(spans.span(5), None);
        let mut other = table();
        other.spans[1] = Span::At(Position { file: 1, line: 121, column: 1 });
        assert!(!spans.is(&other.encode()));
        let empty = SpanTable::default().encode();
        let none = Spans::new(&empty).unwrap();
        assert_eq!((none.files(), none.len(), none.span(0)), (0, 0, None));
    }

    /// The invariant: an item moved down `n` lines is the same table but for its bases. Its
    /// encoding is the one before up to the bases, and each relative span reads `n` lines on
    /// while an absolute one (another file's) stays. The control: a span's own change is a
    /// change before the bases.
    #[test]
    fn a_moved_item_changes_its_bases_and_nothing_else() {
        let was = table();
        let before = was.encode();
        let spans = Spans::new(&before).unwrap();
        let rows_end = before.len() - BASE * was.bases.len();
        let first_change = |after: &[u8]| {
            before.iter().zip(after).position(|(a, b)| a != b).expect("a change")
        };
        for n in [1u32, 3, 200] {
            let mut moved = table();
            for base in &mut moved.bases {
                base.line += n;
            }
            let after = moved.encode();
            assert_eq!(before.len(), after.len());
            assert!(first_change(&after) >= rows_end, "a row moved: {}", first_change(&after));
            assert!(!spans.is(&after));
            let now = Spans::new(&after).unwrap();
            for (at, span) in was.spans.iter().enumerate() {
                let (old, new) = (spans.span(at as u32).unwrap(), now.span(at as u32).unwrap());
                let shift = match span {
                    Span::At(_) => 0,
                    Span::From { .. } => n,
                };
                assert_eq!(new, Position { line: old.line + shift, ..old }, "span {at}");
            }
        }
        let mut edited = table();
        edited.spans[3] = Span::From { base: 0, line: 3, column: 9 };
        assert!(first_change(&edited.encode()) < rows_end, "the control: a row changed");
    }

    /// Checking a table is one allocation whatever its size; reading a span or a file name,
    /// and telling an unchanged table, none.
    #[test]
    fn a_table_is_one_allocation_and_reads_none() {
        for n in [2u32, 5000] {
            let owned = SpanTable {
                files: (0..n).map(|it| format!("src/f{it}.rs")).collect(),
                spans: (0..n)
                    .map(|it| match it % 2 {
                        0 => Span::At(Position { file: it, line: it + 1, column: 1 }),
                        _ => Span::From { base: 0, line: it, column: 1 },
                    })
                    .collect(),
                bases: vec![Position { file: n - 1, line: 1, column: 1 }],
            };
            let bytes = owned.encode();
            let spans = blaze_alloc_count::assert_at_most(1, "a table checked and kept", || {
                Spans::new(&bytes).unwrap()
            });
            blaze_alloc_count::assert_at_most(0, "a span, a file name, a comparison", || {
                assert_eq!(spans.span(n - 1), Some(Position { file: n - 1, line: n, column: 1 }));
                assert_eq!(spans.span(n - 2), Some(Position { file: n - 2, line: n - 1, column: 1 }));
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

    #[test]
    fn position_in_reads_a_relative_span_absolute() {
        let bytes = table().encode();
        let owned = table();
        for index in 0..owned.spans.len() as u32 {
            let (name, at) = position_in(&bytes, index).unwrap();
            let want = owned.spans[index as usize].resolve(&owned.bases).unwrap();
            assert_eq!((name, at), (owned.files[want.file as usize].as_str(), want));
        }
        assert_eq!(position_in(&bytes, 3).unwrap().1, Position { file: 1, line: 42, column: 9 });
        assert!(position_in(&bytes, 5).is_err());
        assert!(position_in(&bytes[..bytes.len() - 1], 4).is_err(), "a base cut off");
    }
}
