//! What the `hir_*` targets share. It is a module of each (`#[path = "hir_shared.rs"] mod
//! shared;`), not a target of its own.
//!
//! The targets read what the front end keeps of a file or a body once it is cold
//! (`crates/hir-def/src/item_tree/persist.rs`, `expr_store/persist.rs`): a `blaze_format`
//! buffer, `Persist::read` then the `Ref`'s `decode`. An input's first byte says what the rest
//! is:
//! - even ([`ARBITRARY`]): a buffer as it came. A decoded value must be written as a buffer
//!   that reads back as an equal value and is written again as the same bytes. It need not be
//!   the input: a reader accepts layouts its writer never makes (an empty slice's offset, words
//!   no table reads, padding), which the store's content hash, not the reader, pins down.
//! - odd ([`COLUMNS`]): edits of one of the target's valid buffers ([`Valid`]), each a byte
//!   set inside a column the decode copies byte for byte. The layout is then the writer's, so a
//!   decoded value must be written as exactly the edited bytes.
//!
//! Either way the read and the decode never panic or hang, and no allocation they make is
//! larger than `blaze_fuzz::allowed` of the buffer's length: each count is checked against the
//! words left, each value is at least one 4-byte word, and no decoded value is 64 bytes per
//! word even in a vector that doubles; the allowance's megabyte is for the interner's first
//! tables.
//!
//! A decoded value holds `SyntaxContext`s and query IDs from the bytes, which only the
//! database that wrote them may look up. The targets compare and drop them and never hand one
//! to a database, nor format one (`Debug` of a query ID asks the attached database).
//!
//! The valid buffers are laid out by [`Build`] in the order the crate's writers lay theirs out,
//! through the same `blaze_format::Writer`: the crate keeps its root records to itself, and its
//! fixtures need its test database. Each target's tests check its buffers decode and are
//! written back as the same bytes, so a change of layout shows there and not as lost coverage.

#![allow(dead_code)]

use std::{collections::HashMap, mem::size_of, ops::Range};

use blaze_format::{Aligned, FormatError, Persist, Record, SchemaHash, Writer};

/// An input that is a buffer as it came.
pub const ARBITRARY: u8 = 0;
/// An input that edits the columns of a valid buffer: which buffer, then edits of three bytes
/// each, a little-endian `u16` place among the columns' bytes and the byte to set there.
pub const COLUMNS: u8 = 1;

/// A buffer the crate's writer makes, and where in it the columns are that its decode copies
/// byte for byte.
pub struct Valid {
    pub bytes: Vec<u8>,
    pub columns: Vec<Range<usize>>,
}

/// Runs one input against `decode`, `valid` being the target's valid buffers.
pub fn check<T: Persist + PartialEq>(
    data: &[u8],
    valid: &[Valid],
    decode: fn(&[u8]) -> Result<T, FormatError>,
) {
    let Some((&mode, rest)) = data.split_first() else { return };
    match mode & 1 {
        0 => arbitrary(rest, decode),
        _ => edited(rest, valid, decode),
    }
}

/// Reads and decodes `bytes`, checking the allocation bound.
fn read<T>(bytes: &[u8], decode: fn(&[u8]) -> Result<T, FormatError>) -> Result<T, FormatError> {
    let buffer = Aligned::copy_from(bytes);
    let (largest, decoded) = blaze_fuzz::largest_allocation(|| decode(&buffer));
    let allowed = blaze_fuzz::allowed(bytes.len());
    assert!(
        largest <= allowed,
        "allocation: decoding {} bytes asked for {largest} bytes at once, past {allowed}",
        bytes.len(),
    );
    decoded
}

fn arbitrary<T: Persist + PartialEq>(bytes: &[u8], decode: fn(&[u8]) -> Result<T, FormatError>) {
    let Ok(value) = read(bytes, decode) else { return };
    let written = blaze_format::to_bytes(&value);
    let again = match read(&written, decode) {
        Ok(again) => again,
        Err(error) => panic!("fixpoint: a decoded value is written as a buffer its reader refuses: {error}"),
    };
    assert!(again == value, "fixpoint: a decoded value, written and read again, is another value");
    assert!(
        blaze_format::to_bytes(&again) == written,
        "fixpoint: a decoded value written twice gives different bytes",
    );
}

/// The valid buffers that have columns to edit, in the order [`check`] picks from.
fn editable(valid: &[Valid]) -> impl Iterator<Item = &Valid> {
    valid.iter().filter(|it| !it.columns.is_empty())
}

fn edited<T: Persist + PartialEq>(rest: &[u8], valid: &[Valid], decode: fn(&[u8]) -> Result<T, FormatError>) {
    let Some((&pick, edits)) = rest.split_first() else { return };
    let count = editable(valid).count();
    let Some(seed) = editable(valid).nth(usize::from(pick) % count.max(1)) else { return };
    let total: usize = seed.columns.iter().map(|it| it.len()).sum();
    let mut bytes = seed.bytes.clone();
    for edit in edits.chunks_exact(3) {
        let at = usize::from(u16::from_le_bytes([edit[0], edit[1]])) % total;
        bytes[place(&seed.columns, at)] = edit[2];
    }
    let Ok(value) = read(&bytes, decode) else { return };
    assert!(
        blaze_format::to_bytes(&value) == bytes,
        "round trip: a buffer edited in its columns decodes to a value not written as that buffer",
    );
}

/// The byte `at` bytes into the columns, counted across them in order.
fn place(columns: &[Range<usize>], mut at: usize) -> usize {
    for column in columns {
        if at < column.len() {
            return column.start + at;
        }
        at -= column.len();
    }
    unreachable!("a place within the columns' length")
}

/// The inputs a target starts from: each valid buffer as it is, each editable one unedited,
/// and each of those with the first and the last byte of every column set to its edges.
pub fn inputs(valid: &[Valid]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> =
        valid.iter().map(|it| [&[ARBITRARY][..], &it.bytes].concat()).collect();
    for (pick, seed) in editable(valid).enumerate() {
        let pick = pick as u8;
        out.push(vec![COLUMNS, pick]);
        let mut first = 0usize;
        for column in &seed.columns {
            let last = first + column.len() - 1;
            for (at, byte) in [(first, 0xff), (first, 0x00), (last, 0x01), (last, 0xff)] {
                let at = (at as u16).to_le_bytes();
                out.push(vec![COLUMNS, pick, at[0], at[1], byte]);
            }
            first = last + 1;
        }
    }
    out
}

/// Where a column was written and how many records it holds.
#[derive(Clone, Copy)]
pub struct Col {
    at: u64,
    len: u32,
}

/// A buffer laid out as a crate's writer lays its out, recording which columns are copied
/// byte for byte on decode.
pub struct Build {
    out: Writer,
    columns: Vec<Range<usize>>,
}

impl Build {
    pub fn new(schema: SchemaHash) -> Build {
        Build { out: Writer::new(schema), columns: Vec::new() }
    }

    /// Writes `values` as a column, as `Column::push` does; `raw` when the decode copies its
    /// bytes as they are, so that an edit of them must come back from the writer.
    pub fn column<T: Record>(&mut self, values: &[T], raw: bool) -> Col {
        let at = self.out.push_slice(values);
        let bytes = values.len() * size_of::<T>();
        if raw && bytes > 0 {
            self.columns.push(at as usize..at as usize + bytes);
        }
        Col { at, len: values.len() as u32 }
    }

    /// The root record, to be written after every column.
    pub fn root(&self) -> Root {
        Root { at: self.out.next::<u32>(), words: Vec::new() }
    }

    pub fn finish(mut self, root: Root) -> Valid {
        let at = self.out.push_slice(&root.words);
        assert_eq!(at, root.at, "a column written after the root was begun");
        Valid { bytes: self.out.finish(at), columns: self.columns }
    }
}

/// A root record of slices and words, aligned to 4 as every root of these buffers is, its
/// fields in their declared order.
pub struct Root {
    at: u64,
    words: Vec<u32>,
}

impl Root {
    fn field_at(&self) -> u64 {
        self.at + (self.words.len() * size_of::<u32>()) as u64
    }

    /// A `Slice` field of `col`, as `Column::slice` makes one: empty is all zeros.
    pub fn slice(&mut self, col: Col) {
        let offset = match col.len {
            0 => 0,
            _ => (col.at as i64 - self.field_at() as i64) as i32,
        };
        self.words.extend([offset as u32, col.len]);
    }

    /// A `WordsRec` field: the words, then the names.
    pub fn words(&mut self, placed: &Placed) {
        self.slice(placed.words);
        self.slice(placed.names);
    }

    pub fn u32(&mut self, word: u32) {
        self.words.push(word);
    }
}

/// The words and names of a buffer, as `hir::persist::Words` writes them: each name once, in
/// the order it is first written.
#[derive(Default)]
pub struct Words {
    words: Vec<u32>,
    text: String,
    names: Vec<(u32, u32)>,
    by_text: HashMap<String, u32>,
}

/// Where [`Words::place`] wrote the words and the names.
pub struct Placed {
    words: Col,
    names: Col,
}

impl Words {
    /// The word the next one written will be.
    pub fn at(&self) -> u32 {
        self.words.len() as u32
    }

    pub fn u32(&mut self, word: u32) {
        self.words.push(word);
    }

    pub fn u64(&mut self, value: u64) {
        self.u32(value as u32);
        self.u32((value >> 32) as u32);
    }

    /// A name or a symbol: its index in the table.
    pub fn name(&mut self, text: &str) {
        let index = match self.by_text.get(text) {
            Some(&index) => index,
            None => {
                let index = self.names.len() as u32;
                self.names.push((self.text.len() as u32, text.len() as u32));
                self.text.push_str(text);
                self.by_text.insert(text.to_owned(), index);
                index
            }
        };
        self.u32(index);
    }

    /// Writes the text, the names and the words, as `Words::place` does. None of it is a column
    /// the decode copies: names are interned again, and words are read as values.
    pub fn place(&self, build: &mut Build) -> Placed {
        let text_at = build.column(self.text.as_bytes(), false).at;
        let names_at = build.out.next::<[u32; 2]>();
        let names: Vec<[u32; 2]> = (self.names.iter().enumerate())
            .map(|(index, &(start, len))| {
                let field_at = names_at + (index * 8) as u64;
                let offset = (text_at + u64::from(start)) as i64 - field_at as i64;
                [offset as i32 as u32, len]
            })
            .collect();
        let names = build.column(&names, false);
        assert_eq!(names.at, names_at, "`Writer::next` is not where `push_slice` writes");
        let words = build.column(&self.words, false);
        Placed { words, names }
    }
}
