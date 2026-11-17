//! One crate's kept expansions as ONE set with ONE root: the entries are rows over columns the
//! whole set shares, and every token text is stored once per set.
//!
//! The form it replaces ([`MAGIC_V1`]) held each entry as a whole self-describing expansion
//! record (a blaze-format header and a 14-column root, about 400 of an entry's 2,500 bytes on
//! tempo), each token's text again in every entry, and each context's 32-byte keys again in
//! every entry. Here a set is, little-endian, every field fixed-width:
//!
//! ```text
//! head      magic (8)  count (u32)  atoms (u32)  atom text length (u32)  ids (u32)
//!           each column's byte length (u32 x COLS)
//! keys      count x 32: the call keys, ascending, no two equal
//! rows      count x COLS x u32: where entry i ends in each column (it starts where i-1 ends)
//! atoms     the token atom table of [`crate::atoms`] (the one home of the token row form):
//!           atoms x ATOM bytes (word, text end, suffix length), each distinct token once, in
//!           first-use order over the entries in key order
//! text      the atoms' texts
//! ids       ids x 32: the contexts' keys, each distinct one once, in first-use order
//! columns   COLS columns, each the entries' parts in key order
//! ```
//!
//! The columns ([`SetColumn`]): `Asked` (per entry its atom count, u32, then the atoms as
//! [`crate::kept::Asked`] writes them), `Tokens` (a token row of [`crate::atoms`], a u32 atom
//! id, per token), `Contexts` (per node its parent LEB128, its flags byte, then its keys as
//! LEB128 ids), and the expansion record's own columns kept byte for byte (each token's
//! context, the origins, nested calls, holes, reads, their text, diagnostics, notes and the
//! arm): those name tokens, notes and texts by index inside their entry, so a range of them is
//! a whole value. A literal's suffix length is in its atom, so a row alone is a whole token.
//!
//! An entry costs 32 bytes of key and `4 x COLS` of row beside its tokens' 4-byte atom ids:
//! no root, no padding, no text. [`KeptSet::open`] checks the head, every section's length
//! (a set cut anywhere is [`KeptError::Short`]), the key order and every row and atom range
//! once; an entry's bytes are decoded only when it is asked for, and a byte this crate does not
//! write is a typed [`RecordError`] naming the column, never a panic. The bytes are a function
//! of the entries (rule 25): sorted by key, the atom and id tables in first-use order over the
//! sorted entries, whatever order the entries were made in; two entries of one key with
//! different values are [`KeptError::TwoValues`] (rule 21).

use blaze_format::leb;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Vec};

use crate::{
    atoms::{ATOM, Atoms, AtomsBuilder, AtomsError},
    expansion::Expansion,
    kept::{AskedIter, KeptEntry, KeptError},
    key::CallKey,
    record::{COLUMNS, RecordColumn, RecordError, ctx_of, edition_of, token_of},
    tokens::{Ctx, CtxNode, DefSite, Mark, MarkOf, TokenKind, TokenStream, Transparency, index},
};

/// This form's magic: the set's version. A step of the layout steps it.
pub const MAGIC: [u8; 8] = *b"BZXSET3\0";
/// The first form: one whole expansion record per entry. Detected and rebuilt
/// ([`KeptError::OldForm`]), never misread.
pub const MAGIC_V1: [u8; 8] = *b"BZXSET1\0";
/// The second form: this one with 8-byte atoms and a per-entry suffix column. Detected and
/// rebuilt, never misread.
pub const MAGIC_V2: [u8; 8] = *b"BZXSET2\0";

/// The atom builder's hash seed. It only spreads the builder's lookups: no byte of a set
/// depends on it ([`AtomsBuilder`]), so it is a constant, not an input.
const ATOM_SEED: u64 = 0x6b65_7074_2073_6574;

/// The set's columns, in the head's and each row's order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetColumn {
    Asked,
    Tokens,
    Ctxs,
    Contexts,
    Origins,
    Nested,
    Holes,
    Reads,
    ReadText,
    Diagnostics,
    Notes,
    Arm,
}

const COLS: usize = 12;
const KEY: usize = 32;
const ID: usize = 32;
const HEAD: usize = 8 + 4 * 4 + COLS * 4;

/// The columns kept byte for byte from an expansion record: the set's column and the record's.
const VERBATIM: [(SetColumn, RecordColumn); 9] = [
    (SetColumn::Ctxs, RecordColumn::Ctxs),
    (SetColumn::Origins, RecordColumn::Origins),
    (SetColumn::Nested, RecordColumn::Nested),
    (SetColumn::Holes, RecordColumn::Holes),
    (SetColumn::Reads, RecordColumn::Reads),
    (SetColumn::ReadText, RecordColumn::ReadText),
    (SetColumn::Diagnostics, RecordColumn::Diagnostics),
    (SetColumn::Notes, RecordColumn::Notes),
    (SetColumn::Arm, RecordColumn::Arm),
];

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn wide(value: u32) -> usize {
    usize::try_from(value).expect("a u32 fits a usize on every target blaze runs on")
}

/// How many 32-byte keys a context node with `flags` holds: its outer mark's (bit 0), its
/// crate's, its module's (bit 5), as the expansion record writes them.
fn keys_of(flags: u8) -> usize {
    usize::from(flags & 1) + 1 + usize::from(flags >> 5 & 1)
}

fn oom<E>(_: E) -> KeptError {
    KeptError::OutOfMemory
}

fn narrow(value: usize) -> Result<u32, KeptError> {
    u32::try_from(value).map_err(|_| KeptError::TooLarge)
}

/// A set read in place: its sections checked once by [`KeptSet::open`]; nothing is decoded
/// until an entry is asked for.
#[derive(Clone, Copy, Debug)]
pub struct KeptSet<'k> {
    count: usize,
    keys: &'k [u8],
    rows: &'k [u8],
    /// The token atoms, proved once by [`Atoms::of`] in `open`.
    atoms: Atoms<'k>,
    ids: &'k [u8],
    columns: [&'k [u8]; COLS],
}

impl<'k> KeptSet<'k> {
    /// The set `bytes` hold ([`write_set`]'s). Checked once: the magic (an earlier form is
    /// [`KeptError::OldForm`]), every section's length against the buffer (exactly: a cut set
    /// is [`KeptError::Short`], bytes past the last column [`KeptError::Trailing`]), the keys'
    /// order, every row's ranges and every atom's text range. Any alignment.
    pub fn open(bytes: &'k [u8]) -> Result<KeptSet<'k>, KeptError> {
        if bytes.len() < 8 {
            return Err(KeptError::Short { what: "its magic" });
        }
        if bytes[..8] == MAGIC_V1 || bytes[..8] == MAGIC_V2 {
            return Err(KeptError::OldForm);
        }
        if bytes[..8] != MAGIC {
            return Err(KeptError::Magic);
        }
        if bytes.len() < HEAD {
            return Err(KeptError::Short { what: "its head" });
        }
        let count = wide(u32_at(bytes, 8));
        let atoms = wide(u32_at(bytes, 12));
        let text_len = wide(u32_at(bytes, 16));
        let ids = wide(u32_at(bytes, 20));
        let mut lens = [0usize; COLS];
        for (column, len) in lens.iter_mut().enumerate() {
            *len = wide(u32_at(bytes, 24 + column * 4));
        }
        let mut at = HEAD;
        let mut take = |len: Option<usize>, what: &'static str| -> Result<&'k [u8], KeptError> {
            let end = len.and_then(|len| at.checked_add(len)).ok_or(KeptError::TooLarge)?;
            if end > bytes.len() {
                return Err(KeptError::Short { what });
            }
            let part = &bytes[at..end];
            at = end;
            Ok(part)
        };
        let keys = take(count.checked_mul(KEY), "its keys")?;
        let rows = take(count.checked_mul(COLS * 4), "its rows")?;
        let atom_table = take(atoms.checked_mul(ATOM), "its atom table")?;
        let text = take(Some(text_len), "its atom texts")?;
        let id_table = take(ids.checked_mul(ID), "its context keys")?;
        let mut columns: [&'k [u8]; COLS] = [&[]; COLS];
        for (column, len) in columns.iter_mut().zip(lens) {
            *column = take(Some(len), "its columns")?;
        }
        if at != bytes.len() {
            return Err(KeptError::Trailing);
        }
        for index in 1..count {
            if keys[(index - 1) * KEY..index * KEY] >= keys[index * KEY..(index + 1) * KEY] {
                return Err(KeptError::KeyOrder { index });
            }
        }
        for (column, len) in lens.iter().enumerate() {
            let mut before = 0usize;
            for index in 0..count {
                let end = wide(u32_at(rows, (index * COLS + column) * 4));
                if end < before || end > *len {
                    return Err(KeptError::EntryEnd { index });
                }
                before = end;
            }
            if before != *len {
                return Err(KeptError::Table { what: "a column's bytes past its last entry", index: column });
            }
        }
        let atoms = Atoms::of(atom_table, text).map_err(|error| KeptError::Atoms { error })?;
        Ok(KeptSet { count, keys, rows, atoms, ids: id_table, columns })
    }

    /// How many expansions the set holds.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The key at `index`, in ascending order.
    pub fn key(&self, index: usize) -> CallKey {
        let mut key = [0u8; KEY];
        key.copy_from_slice(&self.keys[index * KEY..(index + 1) * KEY]);
        CallKey(ContentId(key))
    }

    /// Where `key` is in the key table: a binary search over the fixed-width keys.
    pub fn position(&self, key: &CallKey) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.keys[mid * KEY..(mid + 1) * KEY].cmp(&key.0.0[..]) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Entry `index`'s part of `column`. `index` is below [`KeptSet::len`]; its ranges were
    /// checked by `open`.
    fn range(&self, column: SetColumn, index: usize) -> &'k [u8] {
        let end = |at: usize| wide(u32_at(self.rows, (at * COLS + column as usize) * 4));
        let start = if index == 0 { 0 } else { end(index - 1) };
        &self.columns[column as usize][start..end(index)]
    }

    /// Context key `id`; `None` past the table.
    fn id(&self, id: u64) -> Option<ContentId> {
        let at = usize::try_from(id).ok()?.checked_mul(ID)?;
        let bytes = self.ids.get(at..at.checked_add(ID)?)?;
        let mut key = [0u8; ID];
        key.copy_from_slice(bytes);
        Some(ContentId(key))
    }

    /// Entry `index` (below [`KeptSet::len`]), its asked atoms checked; its expansion is read
    /// only when asked ([`SetEntry::expansion`]).
    pub fn entry(&self, index: usize) -> Result<SetEntry<'k>, KeptError> {
        let asked = self.range(SetColumn::Asked, index);
        if asked.len() < 4 {
            return Err(KeptError::Short { what: "an entry's asked count" });
        }
        let asked = AskedIter::checked(&asked[4..], wide(u32_at(asked, 0)))?;
        Ok(SetEntry { set: *self, index, asked })
    }

    /// The entry kept under `key`.
    pub fn find(&self, key: &CallKey) -> Result<Option<SetEntry<'k>>, KeptError> {
        match self.position(key) {
            Some(index) => self.entry(index).map(Some),
            None => Ok(None),
        }
    }
}

/// One entry of a set, its asked atoms checked: what a hit reads.
#[derive(Clone, Copy, Debug)]
pub struct SetEntry<'k> {
    set: KeptSet<'k>,
    index: usize,
    asked: AskedIter<'k>,
}

/// A column's bad byte, as the expansion record names one.
fn bad(column: RecordColumn, at: usize, what: &'static str) -> RecordError {
    RecordError::Invalid { column, at, what }
}

fn leb_in(bytes: &[u8], at: &mut usize, column: RecordColumn) -> Result<u64, RecordError> {
    let start = *at;
    leb::read_u64(bytes, at).map_err(|error| bad(column, start, error.as_str()))
}

impl<'k> SetEntry<'k> {
    /// Its index in the set.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The cfg atoms the item walk asked when this was expanded, with their answers then.
    pub fn asked(&self) -> AskedIter<'k> {
        self.asked
    }

    /// The expansion, decoded once from the set's columns into `alloc`: equal to the one that
    /// was kept. A byte this crate does not write is a [`RecordError`] naming the column.
    pub fn expansion<'a>(&self, alloc: Alloc<'a>) -> Result<Expansion<'a>, RecordError> {
        let set = &self.set;
        let mut stream = TokenStream::new_in(alloc);
        let mut nodes: AVec<'a, Ctx> = Vec::new_in(alloc);
        let contexts = set.range(SetColumn::Contexts, self.index);
        let column = RecordColumn::Contexts;
        let mut at = 0usize;
        while at < contexts.len() {
            let start = at;
            let parent = leb_in(contexts, &mut at, column)?;
            let parent = ctx_of(parent, &nodes).ok_or_else(|| bad(column, start, "a parent context not before it"))?;
            let flags = *contexts.get(at).ok_or_else(|| bad(column, at, "cut off by its end"))?;
            at += 1;
            let transparency =
                Transparency::from_byte(flags >> 1 & 0b11).ok_or_else(|| bad(column, start, "a mark of no transparency"))?;
            let edition = edition_of(u64::from(flags >> 3 & 0b11)).ok_or_else(|| bad(column, start, "a mark of no edition"))?;
            if flags >> 6 != 0 {
                return Err(bad(column, start, "a mark's flags with bits set no field uses"));
            }
            let mut key = || -> Result<ContentId, RecordError> {
                let id_at = at;
                let id = leb_in(contexts, &mut at, column)?;
                set.id(id).ok_or_else(|| bad(column, id_at, "a context key id past the set's keys"))
            };
            let of = match flags & 1 {
                0 => MarkOf::This,
                _ => MarkOf::Outer(key()?),
            };
            let krate = CrateItemKey(key()?);
            let module = match flags >> 5 & 1 {
                0 => None,
                _ => Some(ItemKey(key()?)),
            };
            let mark = Mark { of, transparency, edition, site: DefSite { krate, module } };
            let ctx = stream.context(CtxNode { parent, mark }).map_err(|error| RecordError::Stream { column, error })?;
            if ctx.node() != Some(index(nodes.len())) {
                return Err(bad(column, start, "a context the table already holds"));
            }
            nodes.push(ctx);
        }
        // Each token's context first (a LEB128 column), then the rows in one pass through the
        // atom table: room made once ([`Atoms::extend_rows`]), nothing pushed one by one.
        let rows = set.range(SetColumn::Tokens, self.index);
        let ctxs = set.range(SetColumn::Ctxs, self.index);
        let mut token_ctxs: AVec<'a, Ctx> = Vec::new_in(alloc);
        let mut ctx_at = 0usize;
        while ctx_at < ctxs.len() {
            let ctx_start = ctx_at;
            let raw_ctx = leb_in(ctxs, &mut ctx_at, RecordColumn::Ctxs)?;
            let ctx = ctx_of(raw_ctx, &nodes).ok_or_else(|| bad(RecordColumn::Ctxs, ctx_start, "a token's context its table does not have"))?;
            token_ctxs.push(ctx);
        }
        if token_ctxs.len() * 4 != rows.len() {
            return Err(bad(RecordColumn::Ctxs, ctx_at, "a context count that is not the token count"));
        }
        set.atoms.extend_rows(&mut stream, rows, |token| token_ctxs[token]).map_err(|error| match error {
            AtomsError::Stream(error) => RecordError::Stream { column: RecordColumn::Tokens, error },
            AtomsError::NoSuchAtom { row, .. } => bad(RecordColumn::Tokens, row * 4, "an atom id past the set's atoms"),
            _ => bad(RecordColumn::Tokens, rows.len(), "token rows cut off by the column's end"),
        })?;
        let mut rest: [&[u8]; COLUMNS] = [&[]; COLUMNS];
        for (set_column, record_column) in VERBATIM {
            if set_column != SetColumn::Ctxs {
                rest[record_column as usize] = set.range(set_column, self.index);
            }
        }
        Expansion::read_after_tokens(rest, stream, alloc)
    }
}

/// One expansion handed to [`write_set`]: made this solve (its [`crate::kept::write_entry`]
/// bytes) or found again in the earlier set.
#[derive(Clone, Copy, Debug)]
pub enum SetSource<'s> {
    Entry(&'s [u8]),
    Kept(SetEntry<'s>),
}

/// The planted bug of the tests ([`write_set_planted`]): nothing else writes one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetPlant {
    None,
    /// The first entry's `Tokens` row ends one token late: its expansion takes the second
    /// entry's first token.
    RowOffByOne,
}

/// One entry's parts while the set is written: what is copied as it is, and how many of the
/// gathered uses are its.
#[derive(Clone, Copy)]
struct Shape<'s> {
    asked: &'s [u8],
    asked_count: u32,
    tokens: usize,
    nodes: usize,
    verbatim: [&'s [u8]; VERBATIM.len()],
}

/// The uses gathered over the entries in key order: each token interned in the one atom table
/// (its id, in interning order), each context node's (parent, flags), each context key.
struct Uses<'b> {
    atoms: AtomsBuilder<'b>,
    tokens: AVec<'b, u32>,
    nodes: AVec<'b, (u64, u8)>,
    ids: AVec<'b, [u8; ID]>,
}

/// Entry `index`'s parts from `source`, its tokens interned and its other uses pushed onto
/// `uses`. A made entry's columns are the record's (a literal's suffix length from the record's
/// suffix column, into its atom); a kept one's are the earlier set's, its atoms and key ids
/// looked up there.
fn gather<'s>(source: SetSource<'s>, index: usize, uses: &mut Uses<'_>) -> Result<Shape<'s>, KeptError> {
    let bad = |what: &'static str| KeptError::Table { what, index };
    let atoms_error = |error| KeptError::Atoms { error };
    let (tokens_before, nodes_before) = (uses.tokens.len(), uses.nodes.len());
    let mut verbatim: [&'s [u8]; VERBATIM.len()] = [&[]; VERBATIM.len()];
    let (asked, asked_count) = match source {
        SetSource::Entry(bytes) => {
            let entry = KeptEntry::open(bytes)?;
            let columns = entry.columns();
            let words = columns[RecordColumn::Tokens as usize];
            let (lens, text) = (columns[RecordColumn::TextLens as usize], columns[RecordColumn::Text as usize]);
            let suffixes = columns[RecordColumn::Suffixes as usize];
            if words.len() % 4 != 0 {
                return Err(bad("an entry's token entries cut off"));
            }
            let (mut len_at, mut text_at, mut suffix_at) = (0usize, 0usize, 0usize);
            for word in words.chunks_exact(4) {
                let word = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
                let len = leb::read_u64(lens, &mut len_at).ok().and_then(|len| usize::try_from(len).ok());
                let end = len.and_then(|len| text_at.checked_add(len)).filter(|&end| end <= text.len());
                let end = end.ok_or_else(|| bad("an entry's token texts"))?;
                let token_text = core::str::from_utf8(&text[text_at..end]).map_err(|_| bad("an entry's token text that is not UTF-8"))?;
                let mut kind = token_of(word).ok_or_else(|| bad("an entry's token word"))?;
                if let TokenKind::Literal { kind: lit, spacing, .. } = kind {
                    let suffix_len = leb::read_u64(suffixes, &mut suffix_at).ok().and_then(|len| u32::try_from(len).ok());
                    let suffix_len = suffix_len.ok_or_else(|| bad("an entry's literal suffixes"))?;
                    kind = TokenKind::Literal { kind: lit, suffix_len, spacing };
                }
                let id = uses.atoms.intern(kind, token_text).map_err(atoms_error)?;
                uses.tokens.try_push(id).map_err(oom)?;
                text_at = end;
            }
            if len_at != lens.len() || text_at != text.len() || suffix_at != suffixes.len() {
                return Err(bad("an entry's token texts or suffixes past its tokens"));
            }
            let contexts = columns[RecordColumn::Contexts as usize];
            let mut at = 0usize;
            while at < contexts.len() {
                let parent = leb::read_u64(contexts, &mut at).map_err(|_| bad("an entry's contexts"))?;
                let flags = *contexts.get(at).ok_or_else(|| bad("an entry's contexts cut off"))?;
                at += 1;
                uses.nodes.try_push((parent, flags)).map_err(oom)?;
                for _ in 0..keys_of(flags) {
                    let key = contexts.get(at..at + ID).ok_or_else(|| bad("an entry's context keys cut off"))?;
                    let mut id = [0u8; ID];
                    id.copy_from_slice(key);
                    uses.ids.try_push(id).map_err(oom)?;
                    at += ID;
                }
            }
            for (place, (_, record_column)) in verbatim.iter_mut().zip(VERBATIM) {
                *place = columns[record_column as usize];
            }
            let (asked, count) = entry.asked().raw();
            (asked, narrow(count)?)
        }
        SetSource::Kept(entry) => {
            let set = &entry.set;
            let atoms = set.range(SetColumn::Tokens, entry.index);
            if atoms.len() % 4 != 0 {
                return Err(bad("a kept entry's atom ids cut off"));
            }
            for id in atoms.chunks_exact(4) {
                let atom = set.atoms.atom(u32::from_le_bytes([id[0], id[1], id[2], id[3]]));
                let (kind, text) = atom.ok_or_else(|| bad("a kept entry's atom id"))?;
                let id = uses.atoms.intern(kind, text).map_err(atoms_error)?;
                uses.tokens.try_push(id).map_err(oom)?;
            }
            let contexts = set.range(SetColumn::Contexts, entry.index);
            let mut at = 0usize;
            while at < contexts.len() {
                let parent = leb::read_u64(contexts, &mut at).map_err(|_| bad("a kept entry's contexts"))?;
                let flags = *contexts.get(at).ok_or_else(|| bad("a kept entry's contexts cut off"))?;
                at += 1;
                uses.nodes.try_push((parent, flags)).map_err(oom)?;
                for _ in 0..keys_of(flags) {
                    let id = leb::read_u64(contexts, &mut at).map_err(|_| bad("a kept entry's context key ids"))?;
                    let key = set.id(id).ok_or_else(|| bad("a kept entry's context key id"))?;
                    uses.ids.try_push(key.0).map_err(oom)?;
                }
            }
            for (place, (set_column, _)) in verbatim.iter_mut().zip(VERBATIM) {
                *place = set.range(set_column, entry.index);
            }
            let (asked, count) = entry.asked.raw();
            (asked, narrow(count)?)
        }
    };
    Ok(Shape {
        asked,
        asked_count,
        tokens: uses.tokens.len() - tokens_before,
        nodes: uses.nodes.len() - nodes_before,
        verbatim,
    })
}

/// Whether two expansions under one key are one value: equal entry bytes, one kept entry, or
/// (a made one beside a kept one) equal asked atoms and equal expansions, decoded in `alloc`.
fn same(key: CallKey, a: SetSource<'_>, b: SetSource<'_>, alloc: Alloc<'_>) -> Result<bool, KeptError> {
    match (a, b) {
        (SetSource::Entry(x), SetSource::Entry(y)) => return Ok(x == y),
        (SetSource::Kept(x), SetSource::Kept(y)) if x.index == y.index => return Ok(true),
        _ => {}
    }
    let unreadable = |_| KeptError::Unreadable { key };
    let (asked_a, expansion_a) = match a {
        SetSource::Entry(bytes) => {
            let entry = KeptEntry::open(bytes)?;
            (entry.asked(), entry.expansion(alloc).map_err(unreadable)?)
        }
        SetSource::Kept(entry) => (entry.asked(), entry.expansion(alloc).map_err(unreadable)?),
    };
    let (asked_b, expansion_b) = match b {
        SetSource::Entry(bytes) => {
            let entry = KeptEntry::open(bytes)?;
            (entry.asked(), entry.expansion(alloc).map_err(unreadable)?)
        }
        SetSource::Kept(entry) => (entry.asked(), entry.expansion(alloc).map_err(unreadable)?),
    };
    Ok(Iterator::eq(asked_a, asked_b) && expansion_a == expansion_b)
}

/// For each of `uses`, its id; and for each id, its first use. Equal uses share an id; ids
/// count up in the order of each value's first use, so the table is a function of the uses'
/// sequence alone (rule 25). No hashing: one sort of the use indices.
fn intern<'b, T: Ord>(uses: &[T], alloc: Alloc<'b>) -> Result<(AVec<'b, u32>, AVec<'b, u32>), KeptError> {
    let n = uses.len();
    let n32 = narrow(n)?;
    let mut order: AVec<'b, u32> = Vec::try_with_capacity_in(n, alloc).map_err(oom)?;
    for use_at in 0..n32 {
        order.try_push(use_at).map_err(oom)?;
    }
    order.sort_unstable_by(|&a, &b| uses[wide(a)].cmp(&uses[wide(b)]).then(a.cmp(&b)));
    // `group[u]`: the group of equal values use `u` is in, numbered in value order; `firsts[g]`:
    // group `g`'s first use (the first in `order`, which ties by use index).
    let mut group: AVec<'b, u32> = Vec::try_with_capacity_in(n, alloc).map_err(oom)?;
    for _ in 0..n {
        group.try_push(0).map_err(oom)?;
    }
    let mut firsts: AVec<'b, u32> = Vec::new_in(alloc);
    for (rank, &use_at) in order.iter().enumerate() {
        if rank == 0 || uses[wide(order[rank - 1])] != uses[wide(use_at)] {
            firsts.try_push(use_at).map_err(oom)?;
        }
        group[wide(use_at)] = narrow(firsts.len() - 1)?;
    }
    // Groups in first-use order: that position is the id.
    let groups = firsts.len();
    let mut by_first: AVec<'b, u32> = Vec::try_with_capacity_in(groups, alloc).map_err(oom)?;
    for g in 0..narrow(groups)? {
        by_first.try_push(g).map_err(oom)?;
    }
    by_first.sort_unstable_by_key(|&g| firsts[wide(g)]);
    let mut id_of: AVec<'b, u32> = Vec::try_with_capacity_in(groups, alloc).map_err(oom)?;
    for _ in 0..groups {
        id_of.try_push(0).map_err(oom)?;
    }
    let mut first_of: AVec<'b, u32> = Vec::try_with_capacity_in(groups, alloc).map_err(oom)?;
    for (id, &g) in by_first.iter().enumerate() {
        id_of[wide(g)] = narrow(id)?;
        first_of.try_push(firsts[wide(g)]).map_err(oom)?;
    }
    for place in group.iter_mut() {
        *place = id_of[wide(*place)];
    }
    Ok((group, first_of))
}

fn put(out: &mut AVec<'_, u8>, bytes: &[u8]) -> Result<(), KeptError> {
    out.try_extend_from_slice(bytes).map_err(oom)
}

fn put_leb(out: &mut AVec<'_, u8>, value: u64) -> Result<(), KeptError> {
    let mut place = [0u8; leb::MAX_LEN];
    let len = leb::put_u64(&mut place, value);
    put(out, &place[..len])
}

/// The set of `entries`, in `alloc` (the caller's frame; every growth fallible,
/// [`KeptError::OutOfMemory`]): sorted here by key, so the bytes do not depend on the order the
/// entries were made in. Two entries of one key with one value are one; with different values,
/// [`KeptError::TwoValues`].
pub fn write_set<'b>(entries: &mut [(CallKey, SetSource<'_>)], alloc: Alloc<'b>) -> Result<AVec<'b, u8>, KeptError> {
    write_set_planted(entries, SetPlant::None, alloc)
}

/// [`write_set`] with a planted bug, for the test that must catch it.
pub fn write_set_planted<'b>(
    entries: &mut [(CallKey, SetSource<'_>)],
    plant: SetPlant,
    alloc: Alloc<'b>,
) -> Result<AVec<'b, u8>, KeptError> {
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut keys: AVec<'b, CallKey> = Vec::new_in(alloc);
    let mut shapes: AVec<'b, Shape<'_>> = Vec::new_in(alloc);
    let mut uses = Uses {
        atoms: AtomsBuilder::new_in(alloc, ATOM_SEED),
        tokens: Vec::new_in(alloc),
        nodes: Vec::new_in(alloc),
        ids: Vec::new_in(alloc),
    };
    let mut at = 0usize;
    while at < entries.len() {
        let (key, first) = entries[at];
        let mut next = at + 1;
        while next < entries.len() && entries[next].0 == key {
            if !same(key, first, entries[next].1, alloc)? {
                return Err(KeptError::TwoValues { key });
            }
            next += 1;
        }
        shapes.try_push(gather(first, keys.len(), &mut uses)?).map_err(oom)?;
        keys.try_push(key).map_err(oom)?;
        at = next;
    }
    // The atoms were interned over the entries in key order: their first-use order is a
    // function of the entries (rule 25).
    let Uses { atoms, tokens: interned, nodes: node_uses, ids: id_uses } = uses;
    let built = atoms.finish_first_use();
    let (key_ids, key_first) = intern(&id_uses, alloc)?;
    let mut columns: [AVec<'b, u8>; COLS] = core::array::from_fn(|_| Vec::new_in(alloc));
    let mut rows: AVec<'b, u8> = Vec::try_with_capacity_in(shapes.len() * COLS * 4, alloc).map_err(oom)?;
    let (mut token, mut node, mut key_use) = (0usize, 0usize, 0usize);
    for (index, shape) in shapes.iter().enumerate() {
        let asked = &mut columns[SetColumn::Asked as usize];
        put(asked, &shape.asked_count.to_le_bytes())?;
        put(asked, shape.asked)?;
        let tokens = &mut columns[SetColumn::Tokens as usize];
        for &id in &interned[token..token + shape.tokens] {
            put(tokens, &built.ids[wide(id)].to_le_bytes())?;
        }
        token += shape.tokens;
        let contexts = &mut columns[SetColumn::Contexts as usize];
        for &(parent, flags) in &node_uses[node..node + shape.nodes] {
            put_leb(contexts, parent)?;
            put(contexts, &[flags])?;
            for _ in 0..keys_of(flags) {
                put_leb(contexts, u64::from(key_ids[key_use]))?;
                key_use += 1;
            }
        }
        node += shape.nodes;
        for (bytes, (set_column, _)) in shape.verbatim.iter().zip(VERBATIM) {
            put(&mut columns[set_column as usize], bytes)?;
        }
        for (column, bytes) in columns.iter().enumerate() {
            let mut end = bytes.len();
            if plant == SetPlant::RowOffByOne && index == 0 && column == SetColumn::Tokens as usize {
                end += 4;
            }
            put(&mut rows, &narrow(end)?.to_le_bytes())?;
        }
    }
    let (atom_table, text) = (&built.atoms, &built.text);
    let mut total = HEAD + keys.len() * KEY + rows.len() + atom_table.len() + text.len() + key_first.len() * ID;
    for column in &columns {
        total += column.len();
    }
    let mut out: AVec<'b, u8> = Vec::try_with_capacity_in(total, alloc).map_err(oom)?;
    put(&mut out, &MAGIC)?;
    for count in [keys.len(), atom_table.len() / ATOM, text.len(), key_first.len()] {
        put(&mut out, &narrow(count)?.to_le_bytes())?;
    }
    for column in &columns {
        put(&mut out, &narrow(column.len())?.to_le_bytes())?;
    }
    for key in keys.iter() {
        put(&mut out, &key.0.0)?;
    }
    put(&mut out, &rows)?;
    put(&mut out, atom_table)?;
    put(&mut out, text)?;
    for &first in key_first.iter() {
        put(&mut out, &id_uses[wide(first)])?;
    }
    for column in &columns {
        put(&mut out, column)?;
    }
    Ok(out)
}
