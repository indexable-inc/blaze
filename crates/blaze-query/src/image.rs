//! The database's part of the daemon image (docs/v2/plan/design/restore.md): five sections of
//! `blaze-image`'s file, read where they are mapped, so a start is an mmap that pages in only
//! what it touches (docs/v2/plan/lanes/db.md section 7).
//!
//! Each section is the database's alone, with a schema of its own; little-endian:
//!
//! - **memos** ([`SectionId::DB_MEMOS`]): [`MEMO_RECORD`] bytes each, sorted by key, a key's
//!   memos in a row;
//! - **edges** ([`SectionId::DB_EDGES`]): [`EDGE_RECORD`] bytes each ([`EdgeRecord`]'s layout,
//!   so read in place), a memo's in the order they were read;
//! - **table** ([`SectionId::DB_TABLE`]): an open-addressed table of `u64`s, a power of two of
//!   them: 1 + the index of a key's first memo, 0 for empty, probed linearly from the key's
//!   slot;
//! - **inputs** ([`SectionId::DB_INPUTS`]): [`INPUT_RECORD`] bytes each: an input key and the
//!   path it names;
//! - **blob** ([`SectionId::DB_BLOB`]): values (each at a 16-byte boundary, read in place),
//!   keys' bytes, paths. Records name offsets into it.
//!
//! Opening reads no byte of them: it finds the five in the spine `blaze-image` checked and
//! checks their lengths. Each group of a section is checked by its CRC32C the first time a read
//! reaches it. A group that fails is [`Corrupt`]: every request reading a key whose memos it
//! held fails naming it (never computing the key again), no image is written while it stands,
//! and it is noted once for the daemon to report. Bytes that pass their CRC
//! but are not what [`write`] writes (a table slot past the memos, edges past the edge section)
//! are a bug in [`write`], and stop the database naming it.

use std::{borrow::Cow, ops::Range, sync::Arc};

use blaze_bytes::le::{self, u32 as u32_at, u64 as u64_at};
use blaze_core::Mapping;
use blaze_format::{SchemaHash, schema};
use blaze_ids::{ContentId, Fingerprint};
use blaze_image::{Builder, Cause, Corrupt, Image, Section, SectionId};
use blaze_reclaim::Guard;

use crate::{
    DbError, Input, QueryKey, QueryKind, kinds,
    memo::{MemoId, Store},
    table::{EDGE_RECORD, EdgeRecord, FxMap, FxSet, ShortLock},
};

pub(crate) const MEMO_RECORD: usize = 192;
pub(crate) const INPUT_RECORD: usize = 64;
const EVICTED: u64 = u64::MAX;

pub(crate) const MEMOS: SchemaHash = schema(
    "blaze v2 db memos 2: memo(kind u16, pad 6, input 32, fingerprint 16, schema 32, producer 32, \
     value id 32, edges from u32, edges u32, value at u64 in blob, value len u64 or max, \
     key at u64 in blob, key len u64) 192; sorted by key, a key's memos in a row",
);
pub(crate) const EDGES: SchemaHash =
    schema("blaze v2 db edges 2: edge(kind u16, pad 6, input 32, saw 16) 56; a memo's in the order read");
pub(crate) const TABLE: SchemaHash =
    schema("blaze v2 db table 2: u64 1 + a key's first memo or 0; a power of two, probed linearly");
pub(crate) const INPUTS: SchemaHash = schema(
    "blaze v2 db inputs 3: input(kind u16, pad 6, input 32, what u8 (0 file, 1 dir, 2 config, \
     3 file at, 4 dir at), pad 7, path at u64 in blob, path len u64) 64; sorted by key; a file \
     at's or dir at's path is its root's 32 bytes, then the path",
);
pub(crate) const BLOB: SchemaHash =
    schema("blaze v2 db blob 2: values at 16-byte boundaries, keys' bytes, paths");

/// What one key contributes to an image: its memos are `memos` of the [`Entries`] it is in.
pub(crate) struct Entry<'g> {
    pub(crate) key: QueryKey,
    pub(crate) memos: Range<usize>,
    pub(crate) key_bytes: Option<&'g [u8]>,
}

/// Every key of an image, sorted, and all their memos in one list (two allocations for the
/// whole image, not one a key).
pub(crate) struct Entries<'g> {
    pub(crate) keys: Vec<Entry<'g>>,
    pub(crate) memos: Vec<MemoId>,
}

fn slot_of(key: &QueryKey, mask: usize) -> usize {
    let mut low = [0; 8];
    low.copy_from_slice(&key.input.0[..8]);
    let mixed = u64::from_le_bytes(low) ^ (u64::from(key.kind.0)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (mixed as usize) & mask
}

fn put_key(record: &mut [u8], key: &QueryKey) {
    record[..2].copy_from_slice(&key.kind.0.to_le_bytes());
    record[8..40].copy_from_slice(&key.input.0);
}

fn put_u64(record: &mut [u8], at: usize, word: u64) {
    record[at..at + 8].copy_from_slice(&word.to_le_bytes());
}

fn path_of(input: &Input) -> (u8, Cow<'_, [u8]>) {
    let at = |root: &ContentId, path: &str| {
        // alloc: once per root input per image.
        let mut bytes = Vec::with_capacity(32 + path.len());
        bytes.extend_from_slice(&root.0);
        bytes.extend_from_slice(path.as_bytes());
        Cow::Owned(bytes)
    };
    match input {
        Input::File(path) => (0, Cow::Borrowed(path.as_bytes())),
        Input::Dir(path) => (1, Cow::Borrowed(path.as_bytes())),
        Input::Config => (2, Cow::Borrowed(&b""[..])),
        Input::FileAt { root, path } => (3, at(root, path)),
        Input::DirAt { root, path } => (4, at(root, path)),
    }
}

/// Where one memo's value went in the blob: at, length (or [`EVICTED`]), content ID.
struct Placed {
    at: u64,
    len: u64,
    id: ContentId,
}

/// Writes the database's five sections of `entries` (sorted by key, each key's memos in a fixed
/// order) and `inputs` into `builder`, calling `symbols(key, value)` on each value written so
/// the caller can mark the symbols it names.
pub(crate) fn write(
    task: &Guard,
    store: &Store,
    all: &Entries<'_>,
    inputs: &[(QueryKey, Input)],
    builder: &mut Builder,
    mut symbols: impl FnMut(QueryKey, &[u8]),
) {
    // The blob first: where each value, key and path went is what the records hold. What a
    // value is (in memory, evicted) is read once, here, so the records agree with the blob
    // even if another thread evicts it meanwhile.
    // alloc: once per image, as are the lists below.
    let mut values: Vec<Placed> = Vec::with_capacity(all.memos.len());
    let mut keys: Vec<(u64, u64)> = Vec::with_capacity(all.keys.len());
    let mut paths: Vec<(u8, u64, u64)> = Vec::with_capacity(inputs.len());
    // Equal values, of any kind, go into the blob once: their memos map the same bytes.
    let mut written: FxMap<ContentId, u64> = FxMap::default(); // alloc: see above
    {
        let mut blob = builder.section(SectionId::DB_BLOB, BLOB);
        for entry in &all.keys {
            keys.push(entry.key_bytes.map_or((0, 0), |bytes| (blob.bytes(bytes), bytes.len() as u64)));
            for &memo in &all.memos[entry.memos.start..entry.memos.end] {
                values.push(match store.bytes(task, memo) {
                    Some(bytes) => {
                        symbols(entry.key, bytes);
                        let id = crate::value_id(bytes);
                        let at = *written.entry(id).or_insert_with(|| {
                            blob.align(16);
                            blob.bytes(bytes)
                        });
                        Placed { at, len: bytes.len() as u64, id }
                    }
                    None => Placed { at: 0, len: EVICTED, id: ContentId::default() },
                });
            }
        }
        for (_, input) in inputs {
            let (what, path) = path_of(input);
            paths.push((what, blob.bytes(&path), path.len() as u64));
        }
    }

    let codes = store.codes();
    {
        let mut out = builder.section(SectionId::DB_MEMOS, MEMOS);
        let mut first_edge = 0u32;
        let mut index = 0;
        for (entry, &(key_at, key_len)) in all.keys.iter().zip(&keys) {
            for &memo in &all.memos[entry.memos.start..entry.memos.end] {
                let hot = store.hot(task, memo);
                let (schema, producer) = codes[hot.code() as usize];
                let edges = u32::try_from(store.edges(task, memo).len()).expect("2^32 edges of one memo");
                let value = &values[index];
                let mut record = [0u8; MEMO_RECORD];
                put_key(&mut record, &entry.key);
                record[40..56].copy_from_slice(&hot.fingerprint().0.to_le_bytes());
                record[56..88].copy_from_slice(&schema.0.0);
                record[88..120].copy_from_slice(&producer.0);
                record[120..152].copy_from_slice(&value.id.0);
                record[152..156].copy_from_slice(&first_edge.to_le_bytes());
                record[156..160].copy_from_slice(&edges.to_le_bytes());
                put_u64(&mut record, 160, value.at);
                put_u64(&mut record, 168, value.len);
                put_u64(&mut record, 176, key_at);
                put_u64(&mut record, 184, key_len);
                out.bytes(&record);
                first_edge = first_edge.checked_add(edges).expect("2^32 edges in one image");
                index += 1;
            }
        }
    }

    {
        let mut out = builder.section(SectionId::DB_EDGES, EDGES);
        for &memo in &all.memos {
            out.bytes(EdgeRecord::as_bytes(store.edges(task, memo)));
        }
    }

    {
        let table_len = (all.keys.len() * 2).next_power_of_two().max(2);
        let mask = table_len - 1;
        let mut table = vec![0u64; table_len]; // alloc: see `values`
        for entry in all.keys.iter().filter(|entry| !entry.memos.is_empty()) {
            let mut slot = slot_of(&entry.key, mask);
            while table[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            table[slot] = entry.memos.start as u64 + 1;
        }
        let mut out = builder.section(SectionId::DB_TABLE, TABLE);
        for slot in table {
            out.bytes(&slot.to_le_bytes());
        }
    }

    let mut out = builder.section(SectionId::DB_INPUTS, INPUTS);
    for ((key, _), &(what, path_at, path_len)) in inputs.iter().zip(&paths) {
        let mut record = [0u8; INPUT_RECORD];
        put_key(&mut record, key);
        record[40] = what;
        put_u64(&mut record, 48, path_at);
        put_u64(&mut record, 56, path_len);
        out.bytes(&record);
    }
}

/// The database's sections of a mapped image. Nothing in them is read until a key is looked
/// up; each group read is checked the first time.
pub(crate) struct ImageMap {
    image: Arc<Image<Mapping>>,
    memos: Section,
    edges: Section,
    table: Section,
    inputs: Section,
    blob: Section,
    memo_count: usize,
    edge_count: usize,
    table_len: usize,
    input_count: usize,
    corrupt: ShortLock<Found>,
}

/// The groups found corrupt: each reported once.
#[derive(Default)]
struct Found {
    seen: FxSet<(SectionId, u32)>,
    first: Option<Corrupt>,
    new: Vec<Corrupt>,
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    ContentId(bytes[at..at + 32].try_into().expect("32 bytes"))
}

fn key_at(bytes: &[u8], at: usize) -> QueryKey {
    QueryKey { kind: QueryKind(le::u16(bytes, at)), input: id_at(bytes, at + 8) }
}

/// One memo of the image, read in place: its edges and value stay in the mapping.
pub(crate) struct ImageMemo<'a> {
    pub(crate) key: QueryKey,
    pub(crate) fingerprint: Fingerprint,
    pub(crate) schema: SchemaHash,
    pub(crate) producer: ContentId,
    pub(crate) edges: &'a [EdgeRecord],
    pub(crate) value: Option<(&'a [u8], ContentId)>,
}

impl ImageMap {
    /// The database's sections of `image`. A section missing or of another schema is
    /// [`DbError::Refused`]; one whose length is not whole records is [`DbError::Invalid`].
    pub(crate) fn open(image: Arc<Image<Mapping>>) -> Result<ImageMap, DbError> {
        let section = |id, schema| image.section(id, schema).map_err(DbError::Refused);
        let (memos, edges, table, inputs, blob) = (
            section(SectionId::DB_MEMOS, MEMOS)?,
            section(SectionId::DB_EDGES, EDGES)?,
            section(SectionId::DB_TABLE, TABLE)?,
            section(SectionId::DB_INPUTS, INPUTS)?,
            section(SectionId::DB_BLOB, BLOB)?,
        );
        let count = |section: &Section, each: usize, what: &'static str| {
            usize::try_from(section.len)
                .ok()
                .filter(|len| len % each == 0)
                .map(|len| len / each)
                .ok_or(DbError::Invalid { section: section.id, what })
        };
        let memo_count = count(&memos, MEMO_RECORD, "not a whole number of memo records")?;
        let edge_count = count(&edges, EDGE_RECORD, "not a whole number of edge records")?;
        let table_len = count(&table, 8, "not a whole number of table slots")?;
        let input_count = count(&inputs, INPUT_RECORD, "not a whole number of input records")?;
        if !table_len.is_power_of_two() {
            return Err(DbError::Invalid { section: table.id, what: "a table whose length is not a power of two" });
        }
        // Sections start at group boundaries (at least 4 KiB into the file), so a base at a
        // 16-byte boundary puts every value and edge record at its own.
        if image.owner().bytes().as_ptr().addr() % 16 != 0 {
            return Err(DbError::Invalid { section: blob.id, what: "mapped at an address off a 16-byte boundary" });
        }
        Ok(ImageMap {
            image,
            memos,
            edges,
            table,
            inputs,
            blob,
            memo_count,
            edge_count,
            table_len,
            input_count,
            corrupt: ShortLock::default(),
        })
    }

    /// `len` bytes at `at` of `section`, their groups checked. A group that fails its CRC is
    /// `Err`; a read past the section is a record naming bytes [`write`] never wrote: a bug.
    fn read(&self, section: &Section, at: u64, len: u64) -> Result<&[u8], Corrupt> {
        self.image.range(section, at, len).map_err(|corrupt| match corrupt.cause {
            Cause::Crc { .. } | Cause::CrcTable { .. } => corrupt,
            Cause::Bounds { .. } | Cause::Misaligned { .. } => {
                panic!("the daemon image's records name bytes the database never wrote: {corrupt}")
            }
        })
    }

    /// Notes `corrupt` for [`ImageMap::take_corrupt`], once a group.
    pub(crate) fn note(&self, corrupt: Corrupt) {
        let mut found = self.corrupt.lock();
        if found.seen.insert((corrupt.section, corrupt.group)) {
            found.first.get_or_insert(corrupt);
            found.new.push(corrupt);
        }
    }

    /// The first group ever found corrupt.
    pub(crate) fn first_corrupt(&self) -> Option<Corrupt> {
        self.corrupt.lock().first
    }

    /// The groups found corrupt since the last call, each once ever.
    pub(crate) fn take_corrupt(&self) -> Vec<Corrupt> {
        std::mem::take(&mut self.corrupt.lock().new)
    }

    fn table_slot(&self, slot: usize) -> Result<u64, Corrupt> {
        Ok(u64_at(self.read(&self.table, slot as u64 * 8, 8)?, 0))
    }

    fn memo_key(&self, index: usize) -> Result<QueryKey, Corrupt> {
        Ok(key_at(self.read(&self.memos, (index * MEMO_RECORD) as u64, 40)?, 0))
    }

    fn checked_memo(&self, first: u64) -> usize {
        match usize::try_from(first - 1) {
            Ok(index) if index < self.memo_count => index,
            _ => panic!("the daemon image's table names memo {} of {}: a bug in its writer", first - 1, self.memo_count),
        }
    }

    /// The indices of `key`'s memos; `Err` when a group they are found through is corrupt.
    pub(crate) fn find(&self, key: &QueryKey) -> Result<Option<Range<usize>>, Corrupt> {
        let mask = self.table_len - 1;
        let mut slot = slot_of(key, mask);
        for _ in 0..self.table_len {
            let first = self.table_slot(slot)?;
            if first == 0 {
                return Ok(None);
            }
            let first = self.checked_memo(first);
            if self.memo_key(first)? == *key {
                let mut end = first + 1;
                while end < self.memo_count && self.memo_key(end)? == *key {
                    end += 1;
                }
                return Ok(Some(first..end));
            }
            slot = (slot + 1) & mask;
        }
        Ok(None)
    }

    /// Calls `f` on every key with memos in the image whose table slot and first memo read
    /// whole; a corrupt group is noted and its keys skipped (they have no image memo).
    pub(crate) fn for_each_key(&self, f: impl FnMut(QueryKey)) {
        self.for_each_key_in(0..self.table_len, f);
    }

    /// How many table slots a group holds: [`ImageMap::for_each_key_in`]'s ranges start and end
    /// on a multiple of it.
    pub(crate) fn slots_per_group(&self) -> usize {
        (self.image.group_len() / 8) as usize
    }

    /// How many slots the image's table has.
    pub(crate) fn table_len(&self) -> usize {
        self.table_len
    }

    /// [`ImageMap::for_each_key`] of the keys whose first memo the table's `slots` name: ranges
    /// on group boundaries split the table among workers, each key in one range.
    pub(crate) fn for_each_key_in(&self, slots: Range<usize>, mut f: impl FnMut(QueryKey)) {
        let per_group = self.slots_per_group();
        let mut slot = slots.start;
        let end = slots.end.min(self.table_len);
        while slot < end {
            let first = match self.table_slot(slot) {
                Ok(first) => first,
                Err(corrupt) => {
                    self.note(corrupt);
                    slot = (slot / per_group + 1) * per_group;
                    continue;
                }
            };
            slot += 1;
            if first == 0 {
                continue;
            }
            match self.memo_key(self.checked_memo(first)) {
                Ok(key) => f(key),
                Err(corrupt) => self.note(corrupt),
            }
        }
    }

    /// The memo at `index`, read in place.
    pub(crate) fn memo(&self, index: usize) -> Result<ImageMemo<'_>, Corrupt> {
        let record = self.read(&self.memos, (index * MEMO_RECORD) as u64, MEMO_RECORD as u64)?;
        let key = key_at(record, 0);
        let fingerprint = Fingerprint(u128::from_le_bytes(record[40..56].try_into().expect("16 bytes")));
        let schema = SchemaHash(id_at(record, 56));
        let producer = id_at(record, 88);
        let value_id = id_at(record, 120);
        let first_edge = u32_at(record, 152) as usize;
        let edge_len = u32_at(record, 156) as usize;
        if first_edge.checked_add(edge_len).is_none_or(|end| end > self.edge_count) {
            panic!("the daemon image's memo {index} names edges past its edge section: a bug in its writer");
        }
        let bytes = self.read(&self.edges, (first_edge * EDGE_RECORD) as u64, (edge_len * EDGE_RECORD) as u64)?;
        // SAFETY: the edge section starts at a group boundary of a mapping `open` found at a
        // 16-byte boundary, and records are 56 bytes, so each is at an 8-byte boundary; a memo
        // of no edges reads an empty range, which `from_bytes` takes at any address.
        let edges = unsafe { EdgeRecord::from_bytes(bytes) };
        let (value_at, value_len) = (u64_at(record, 160), u64_at(record, 168));
        let value = if value_len == EVICTED {
            None
        } else {
            if value_at % 16 != 0 {
                panic!("the daemon image's memo {index} has its value off a 16-byte boundary: a bug in its writer");
            }
            Some((self.read(&self.blob, value_at, value_len)?, value_id))
        };
        Ok(ImageMemo { key, fingerprint, schema, producer, edges, value })
    }

    /// The bytes of the key of the memo at `index`, if it was kept.
    pub(crate) fn key_bytes(&self, index: usize) -> Result<Option<&[u8]>, Corrupt> {
        let record = self.read(&self.memos, (index * MEMO_RECORD + 176) as u64, 16)?;
        let (at, len) = (u64_at(record, 0), u64_at(record, 8));
        if len == 0 {
            return Ok(None);
        }
        Ok(Some(self.read(&self.blob, at, len)?))
    }

    /// Every input the image's memos were checked against: read whole at start.
    pub(crate) fn inputs(&self) -> Result<Vec<(QueryKey, Input)>, Corrupt> {
        // alloc: once when an image is opened, as is each path.
        let mut inputs = Vec::with_capacity(self.input_count);
        for index in 0..self.input_count {
            let record = self.read(&self.inputs, (index * INPUT_RECORD) as u64, INPUT_RECORD as u64)?;
            let key = key_at(record, 0);
            let path = self.read(&self.blob, u64_at(record, 48), u64_at(record, 56))?;
            let (root, path) = match record[40] {
                3 | 4 if path.len() >= 32 => (Some(id_at(path, 0)), &path[32..]),
                3 | 4 => panic!("the daemon image's input {index} has no root before its path: a bug in its writer"),
                _ => (None, path),
            };
            let path = std::str::from_utf8(path).unwrap_or_else(|_| {
                panic!("the daemon image's input {index} has a path that is not UTF-8: a bug in its writer")
            });
            let input = match (record[40], key.kind, root) {
                (0, kinds::FILE, _) => Input::File(path.to_owned()), // alloc: see above
                (1, kinds::DIR, _) => Input::Dir(path.to_owned()),   // alloc: see above
                (2, kinds::CONFIG, _) => Input::Config,
                (3, kinds::FILE_AT, Some(root)) => Input::FileAt { root, path: path.to_owned() }, // alloc: see above
                (4, kinds::DIR_AT, Some(root)) => Input::DirAt { root, path: path.to_owned() },   // alloc: see above
                (what, kind, _) => panic!(
                    "the daemon image's input {index} is a {what} of kind {}: a bug in its writer",
                    kind.0
                ),
            };
            inputs.push((key, input));
        }
        Ok(inputs)
    }
}

#[cfg(test)]
pub(crate) fn encode_edge(edge: &crate::Edge) -> Vec<u8> {
    let mut out = vec![0; EDGE_RECORD];
    put_key(&mut out, &edge.read);
    out[40..56].copy_from_slice(&edge.saw.0.to_le_bytes());
    out
}
