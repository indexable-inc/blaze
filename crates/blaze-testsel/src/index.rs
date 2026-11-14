//! A test suite's index (docs/v2/plan/design/test-selection.md, section 4): what lets an edit
//! touch only the tests that asked what it changed, read in place.
//!
//! - The head (blaze-keyed's `TestIndex`, filed by content; the suite's current head is a root the
//!   host keeps): the program state its rows hold at, the key of its postings, the read table
//!   (every outside read a row asks, as a shape of read asks, and each one's answer when the head
//!   was filed), one fixed-width [`Row`] per kept test sorted by base, and the inline asks of the
//!   rows whose asks are not the postings' (the delta).
//! - The postings (`TestPostings`, filed by content): each ask, sorted by id, with the tests that
//!   asked it (posting ids, each naming a base). Rewritten only at compaction, when the delta
//!   passes half of it.
//!
//! Layouts, all little-endian:
//!
//! ```text
//! head:     "BZTI" version pad[3] | program[32] | postings[32] (ABSENT: none)
//!           | reads_len u32 | reads shape[reads_len] | digest[32] x reads
//!           | rows u32 | Row[ROW] x rows | inline u32 | ask id[32] x inline
//! row:      base[32] address[32] shape[32] micros u64 inline_at u32 inline_len u32 basis u8 pad[3]
//! postings: "BZTP" version pad[3] | bases u32 | base[32] x bases
//!           | entries u32 | (ask[32] start u32 len u32) x entries | ids u32 | id u32 x ids
//! ```
//!
//! Costs: opening either is O(1) after its one check ([`HeadLayout::decode`],
//! [`PostingsLayout::decode`], O(bytes)); a row by base is a binary search, O(log rows); an ask's
//! posting list is a binary search, O(log asks), and O(1) per test in it.

#![blaze::sans_io]

use core::{fmt, ops::Range};
use std::alloc::Allocator;

use blaze_ids::ContentId;

use crate::ask::{ABSENT, Basis, Shape};

const HEAD_MAGIC: &[u8; 4] = b"BZTI";
const POSTINGS_MAGIC: &[u8; 4] = b"BZTP";
const VERSION: u8 = 1;

/// A row's width.
pub const ROW: usize = 32 * 3 + 8 + 4 + 4 + 4;
const ENTRY: usize = 32 + 4 + 4;

/// A row's `inline_at` when its asks are the postings'.
pub const POSTED: u32 = u32::MAX;

/// One kept test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    /// Its base key (`TestRun`).
    pub base: ContentId,
    /// The address of its value: what its asks answered when it was kept.
    pub address: ContentId,
    /// The digest of its shape's bytes.
    pub shape: ContentId,
    /// How long its last run took.
    pub micros: u64,
    /// Where its asks start in the head's inline asks, or [`POSTED`].
    pub inline_at: u32,
    pub inline_len: u32,
    pub basis: Basis,
}

impl Row {
    pub fn posted(&self) -> bool {
        self.inline_at == POSTED
    }

    fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        out.extend_from_slice(&self.base.0);
        out.extend_from_slice(&self.address.0);
        out.extend_from_slice(&self.shape.0);
        out.extend_from_slice(&self.micros.to_le_bytes());
        out.extend_from_slice(&self.inline_at.to_le_bytes());
        out.extend_from_slice(&self.inline_len.to_le_bytes());
        out.extend_from_slice(&[self.basis as u8, 0, 0, 0]);
    }

    /// `bytes` is [`ROW`] long and its basis byte was checked by the layout's decode.
    fn decode(bytes: &[u8]) -> Row {
        Row {
            base: id_at(&bytes[0..32]),
            address: id_at(&bytes[32..64]),
            shape: id_at(&bytes[64..96]),
            micros: u64_at(&bytes[96..104]),
            inline_at: u32_at(&bytes[104..108]),
            inline_len: u32_at(&bytes[108..112]),
            basis: Basis::of_byte(bytes[112]).unwrap_or(Basis::Reached),
        }
    }
}

/// Why bytes are not an index record: where, and what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexError {
    pub at: u32,
    pub what: &'static str,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a test index record that does not decode at byte {}: {}", self.at, self.what)
    }
}

/// Where each part of a head's payload is, found by its one check.
#[derive(Clone, Debug)]
pub struct HeadLayout {
    program: ContentId,
    postings: Option<ContentId>,
    reads: Range<usize>,
    reads_count: u32,
    digests: Range<usize>,
    rows: Range<usize>,
    inline: Range<usize>,
}

impl HeadLayout {
    /// Checks `payload` whole: the layout, the read table's shape, rows sorted by base each
    /// once, each basis, each inline range inside the inline asks, and no posted row in a head
    /// with no postings. O(bytes).
    pub fn decode(payload: &[u8]) -> Result<HeadLayout, IndexError> {
        let mut input = Bytes { all: payload, at: 0 };
        let magic = input.take(4, "shorter than a head's magic")?;
        if magic != HEAD_MAGIC {
            return Err(IndexError { at: 0, what: "not a test index head's magic" });
        }
        if input.take(4, "shorter than a head's version")?[0] != VERSION {
            return Err(IndexError { at: 4, what: "a head of another version" });
        }
        let program = input.id("shorter than a head's program")?;
        let postings = input.id("shorter than a head's postings key")?;
        let postings = (postings != ABSENT).then_some(postings);
        let reads_len = input.u32("shorter than a head's read table length")? as usize;
        let reads_at = input.at;
        let reads = input.take(reads_len, "shorter than a head's read table")?;
        let shape = Shape::decode(reads).map_err(|error| IndexError { at: reads_at as u32 + error.at, what: error.what })?;
        if shape.asks().any(|ask| !ask.is_read()) {
            return Err(IndexError { at: reads_at as u32, what: "a read table that asks a function" });
        }
        let digests_at = input.at;
        input.take(shape.len() as usize * 32, "shorter than a head's read digests")?;
        let rows = input.u32("shorter than a head's row count")? as usize;
        let rows_at = input.at;
        input.take(rows * ROW, "shorter than a head's rows")?;
        let inline = input.u32("shorter than a head's inline count")? as usize;
        let inline_at = input.at;
        input.take(inline * 32, "shorter than a head's inline asks")?;
        if input.at != payload.len() {
            return Err(IndexError { at: input.at as u32, what: "bytes after a head's inline asks" });
        }
        let layout = HeadLayout {
            program,
            postings,
            reads: reads_at..reads_at + reads_len,
            reads_count: shape.len(),
            digests: digests_at..digests_at + shape.len() as usize * 32,
            rows: rows_at..rows_at + rows * ROW,
            inline: inline_at..inline_at + inline * 32,
        };
        let mut last: Option<ContentId> = None;
        for at in 0..rows {
            let bytes = &payload[rows_at + at * ROW..rows_at + (at + 1) * ROW];
            let where_ = (rows_at + at * ROW) as u32;
            if Basis::of_byte(bytes[112]).is_none() {
                return Err(IndexError { at: where_ + 112, what: "a row of a basis no basis has" });
            }
            let row = Row::decode(bytes);
            if last.is_some_and(|last| last >= row.base) {
                return Err(IndexError { at: where_, what: "rows out of order by base, or one twice" });
            }
            last = Some(row.base);
            if row.posted() {
                if postings.is_none() {
                    return Err(IndexError { at: where_ + 104, what: "a posted row in a head with no postings" });
                }
            } else if (row.inline_at as usize).checked_add(row.inline_len as usize).is_none_or(|end| end > inline) {
                return Err(IndexError { at: where_ + 104, what: "a row's inline asks past the head's" });
            }
        }
        Ok(layout)
    }

    /// The head `payload` (the bytes this layout was decoded from) read in place: O(1).
    pub fn view<'b>(&self, payload: &'b [u8]) -> HeadView<'b> {
        HeadView {
            program: self.program,
            postings: self.postings,
            reads: &payload[self.reads.clone()],
            reads_count: self.reads_count,
            digests: &payload[self.digests.clone()],
            rows: &payload[self.rows.clone()],
            inline: &payload[self.inline.clone()],
        }
    }
}

/// A head, read in place.
#[derive(Clone, Copy, Debug)]
pub struct HeadView<'b> {
    /// The program state every row holds at: what the program's changes are asked since.
    pub program: ContentId,
    pub postings: Option<ContentId>,
    reads: &'b [u8],
    reads_count: u32,
    digests: &'b [u8],
    rows: &'b [u8],
    inline: &'b [u8],
}

impl<'b> HeadView<'b> {
    /// The read table as a shape's bytes: what the host answers.
    pub fn reads_bytes(&self) -> &'b [u8] {
        self.reads
    }

    /// The read table's asks, in order. Its bytes were checked by the layout's decode.
    pub fn reads(&self) -> crate::ask::Asks<'b> {
        Shape::checked(self.reads).asks()
    }

    pub fn reads_count(&self) -> u32 {
        self.reads_count
    }

    /// The answer of the read table's `at`th read when the head was filed.
    pub fn read_digest(&self, at: u32) -> ContentId {
        let at = at as usize * 32;
        id_at(&self.digests[at..at + 32])
    }

    pub fn rows(&self) -> u32 {
        (self.rows.len() / ROW) as u32
    }

    pub fn row(&self, at: u32) -> Row {
        let at = at as usize * ROW;
        Row::decode(&self.rows[at..at + ROW])
    }

    /// The row of `base`, by binary search.
    pub fn find(&self, base: &ContentId) -> Option<u32> {
        let (mut low, mut high) = (0u32, self.rows());
        while low < high {
            let middle = low + (high - low) / 2;
            let at = middle as usize * ROW;
            let here = id_at(&self.rows[at..at + 32]);
            match here.cmp(base) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    /// The inline asks of `row` (none for a posted row).
    pub fn inline(&self, row: &Row) -> impl Iterator<Item = ContentId> + use<'b> {
        let range = if row.posted() { 0..0 } else { row.inline_at as usize..(row.inline_at + row.inline_len) as usize };
        let inline = self.inline;
        range.map(move |at| id_at(&inline[at * 32..at * 32 + 32]))
    }

    /// How many inline asks the head holds.
    pub fn inline_len(&self) -> u32 {
        (self.inline.len() / 32) as u32
    }
}

/// Writes a head's payload into `out` (cleared first). `reads` is a shape of read asks and
/// `digests` its answers, in order; `rows` are sorted by base; `inline` holds the inline asks the
/// rows name.
pub fn write_head<A: Allocator>(
    out: &mut Vec<u8, A>,
    program: &ContentId,
    postings: Option<&ContentId>,
    reads: &[u8],
    digests: &[ContentId],
    rows: &[Row],
    inline: &[ContentId],
) {
    debug_assert!(rows.windows(2).all(|pair| pair[0].base < pair[1].base), "a head's rows are sorted by base, each once");
    out.clear();
    out.extend_from_slice(HEAD_MAGIC);
    out.extend_from_slice(&[VERSION, 0, 0, 0]);
    out.extend_from_slice(&program.0);
    out.extend_from_slice(&postings.copied().unwrap_or(ABSENT).0);
    out.extend_from_slice(&(reads.len() as u32).to_le_bytes());
    out.extend_from_slice(reads);
    for digest in digests {
        out.extend_from_slice(&digest.0);
    }
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for row in rows {
        row.encode(out);
    }
    out.extend_from_slice(&(inline.len() as u32).to_le_bytes());
    for ask in inline {
        out.extend_from_slice(&ask.0);
    }
}

/// Where each part of a postings payload is, found by its one check.
#[derive(Clone, Debug)]
pub struct PostingsLayout {
    bases: Range<usize>,
    entries: Range<usize>,
    ids: Range<usize>,
}

impl PostingsLayout {
    /// Checks `payload` whole: entries sorted by ask each once, each range inside the ids, each
    /// id naming a base. O(bytes).
    pub fn decode(payload: &[u8]) -> Result<PostingsLayout, IndexError> {
        let mut input = Bytes { all: payload, at: 0 };
        if input.take(4, "shorter than a postings' magic")? != POSTINGS_MAGIC {
            return Err(IndexError { at: 0, what: "not a test postings' magic" });
        }
        if input.take(4, "shorter than a postings' version")?[0] != VERSION {
            return Err(IndexError { at: 4, what: "postings of another version" });
        }
        let bases = input.u32("shorter than a postings' base count")? as usize;
        let bases_at = input.at;
        input.take(bases * 32, "shorter than a postings' bases")?;
        let entries = input.u32("shorter than a postings' entry count")? as usize;
        let entries_at = input.at;
        input.take(entries * ENTRY, "shorter than a postings' entries")?;
        let ids = input.u32("shorter than a postings' id count")? as usize;
        let ids_at = input.at;
        input.take(ids * 4, "shorter than a postings' ids")?;
        if input.at != payload.len() {
            return Err(IndexError { at: input.at as u32, what: "bytes after a postings' ids" });
        }
        let layout = PostingsLayout {
            bases: bases_at..bases_at + bases * 32,
            entries: entries_at..entries_at + entries * ENTRY,
            ids: ids_at..ids_at + ids * 4,
        };
        let view = layout.view(payload);
        let mut last: Option<ContentId> = None;
        for at in 0..view.entries() {
            let (ask, start, len) = view.entry(at);
            let where_ = (entries_at + at as usize * ENTRY) as u32;
            if last.is_some_and(|last| last >= ask) {
                return Err(IndexError { at: where_, what: "postings entries out of order by ask, or one twice" });
            }
            last = Some(ask);
            if (start as usize).checked_add(len as usize).is_none_or(|end| end > ids) {
                return Err(IndexError { at: where_ + 32, what: "a postings entry past the ids" });
            }
        }
        for at in 0..ids {
            let id = u32_at(&payload[ids_at + at * 4..ids_at + at * 4 + 4]);
            if id as usize >= bases {
                return Err(IndexError { at: (ids_at + at * 4) as u32, what: "a posting id that names no base" });
            }
        }
        Ok(layout)
    }

    pub fn view<'b>(&self, payload: &'b [u8]) -> PostingsView<'b> {
        PostingsView {
            bases: &payload[self.bases.clone()],
            entries: &payload[self.entries.clone()],
            ids: &payload[self.ids.clone()],
        }
    }
}

/// Postings, read in place.
#[derive(Clone, Copy, Debug)]
pub struct PostingsView<'b> {
    bases: &'b [u8],
    entries: &'b [u8],
    ids: &'b [u8],
}

impl<'b> PostingsView<'b> {
    pub fn bases(&self) -> u32 {
        (self.bases.len() / 32) as u32
    }

    /// The base posting id `id` names.
    pub fn base(&self, id: u32) -> ContentId {
        let at = id as usize * 32;
        id_at(&self.bases[at..at + 32])
    }

    pub fn entries(&self) -> u32 {
        (self.entries.len() / ENTRY) as u32
    }

    /// The `at`th entry: its ask, and where its posting ids are.
    pub fn entry(&self, at: u32) -> (ContentId, u32, u32) {
        let at = at as usize * ENTRY;
        let bytes = &self.entries[at..at + ENTRY];
        (id_at(&bytes[0..32]), u32_at(&bytes[32..36]), u32_at(&bytes[36..40]))
    }

    /// The entry of `ask`, by binary search.
    pub fn find(&self, ask: &ContentId) -> Option<u32> {
        let (mut low, mut high) = (0u32, self.entries());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.entry(middle).0.cmp(ask) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    /// The posting ids of entry `at`.
    pub fn ids(&self, at: u32) -> impl Iterator<Item = u32> + use<'b> {
        let (_, start, len) = self.entry(at);
        let ids = self.ids;
        (start as usize..(start + len) as usize).map(move |at| u32_at(&ids[at * 4..at * 4 + 4]))
    }

    /// How many posting ids it holds: the postings' size, which compaction is measured against.
    pub fn len(&self) -> u32 {
        (self.ids.len() / 4) as u32
    }
}

/// Writes a postings payload into `out` (cleared first): `bases` by posting id, and `pairs`
/// (ask, posting id) sorted, each once.
pub fn write_postings<A: Allocator>(out: &mut Vec<u8, A>, bases: &[ContentId], pairs: &[(ContentId, u32)]) {
    debug_assert!(pairs.windows(2).all(|pair| pair[0] < pair[1]), "postings pairs are sorted, each once");
    out.clear();
    out.extend_from_slice(POSTINGS_MAGIC);
    out.extend_from_slice(&[VERSION, 0, 0, 0]);
    out.extend_from_slice(&(bases.len() as u32).to_le_bytes());
    for base in bases {
        out.extend_from_slice(&base.0);
    }
    let mut entries = 0u32;
    let mut at = 0;
    while at < pairs.len() {
        entries += 1;
        let ask = pairs[at].0;
        while at < pairs.len() && pairs[at].0 == ask {
            at += 1;
        }
    }
    out.extend_from_slice(&entries.to_le_bytes());
    let mut at = 0;
    while at < pairs.len() {
        let ask = pairs[at].0;
        let start = at;
        while at < pairs.len() && pairs[at].0 == ask {
            at += 1;
        }
        out.extend_from_slice(&ask.0);
        out.extend_from_slice(&(start as u32).to_le_bytes());
        out.extend_from_slice(&((at - start) as u32).to_le_bytes());
    }
    out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    for (_, id) in pairs {
        out.extend_from_slice(&id.to_le_bytes());
    }
}

struct Bytes<'b> {
    all: &'b [u8],
    at: usize,
}

impl<'b> Bytes<'b> {
    fn take(&mut self, len: usize, what: &'static str) -> Result<&'b [u8], IndexError> {
        let end = self.at.checked_add(len).filter(|end| *end <= self.all.len());
        let Some(end) = end else {
            return Err(IndexError { at: self.at as u32, what });
        };
        let bytes = &self.all[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, IndexError> {
        Ok(u32_at(self.take(4, what)?))
    }

    fn id(&mut self, what: &'static str) -> Result<ContentId, IndexError> {
        Ok(id_at(self.take(32, what)?))
    }
}

fn u32_at(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_at(bytes: &[u8]) -> u64 {
    let mut le = [0; 8];
    le.copy_from_slice(&bytes[0..8]);
    u64::from_le_bytes(le)
}

fn id_at(bytes: &[u8]) -> ContentId {
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[0..32]);
    ContentId(id)
}
