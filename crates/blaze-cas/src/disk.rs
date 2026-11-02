//! The store's bytes on disk (docs/v2-store.md, "On disk"): packs of records, the index table and
//! the head. Every number is little-endian, every record 16-byte aligned, so a pack is read in
//! place wherever it is mapped.

use blaze_format::cursor::{Put as _, Reader};
use blaze_ids::ContentId;

use crate::{Key, Kind, crc};

pub(crate) const ALIGN: u64 = 16;

/// The bytes a record of `stored` payload bytes takes in its pack, padding included; `None`
/// for a length no record can have, as an unverified header or index slot may say.
pub(crate) const fn span_of(stored: u64) -> Option<u64> {
    match RECORD_HEADER.checked_add(stored) {
        Some(n) => n.checked_next_multiple_of(ALIGN),
        None => None,
    }
}

/// Every commit leaves each pack a whole number of blocks long, so every append starts and
/// ends on a block and can be written with `O_DIRECT`. The rest of a commit's last block is
/// zeros, which a record header never starts with.
pub(crate) const BLOCK: u64 = 4096;

/// The domain of [`crate::Entry::content`]: what a value's bytes hash to.
pub(crate) const VALUE_DOMAIN: &str = "blaze v2 store value 1";
/// The domain of a chunk's key: a chunk is kept under the hash of its bytes.
pub(crate) const CHUNK_DOMAIN: &str = "blaze v2 store chunk 1";
pub(crate) const DICTIONARY_DOMAIN: &str = "blaze v2 store dictionary 1";

/// What a value's bytes hash to: [`crate::Entry::content`], and what a fetched value is checked
/// against.
pub fn content_of(bytes: &[u8]) -> ContentId {
    ContentId::of(VALUE_DOMAIN, bytes)
}

pub(crate) use blaze_bytes::le::{u32 as u32_at, u64 as u64_at};

pub(crate) fn id_at(bytes: &[u8], at: usize) -> ContentId {
    ContentId(bytes[at..at + 32].try_into().expect("32 bytes"))
}

// Packs.

pub(crate) const PACK_MAGIC: [u8; 8] = *b"BLZPACK\x04";
/// A pack starts with its magic and its unit (the crate version whose records it holds, 0 for
/// what crates share); records of any kind follow.
pub(crate) const PACK_HEADER: u64 = 16;

pub(crate) fn pack_header(unit: u32) -> [u8; PACK_HEADER as usize] {
    let mut out = [0; PACK_HEADER as usize];
    out[..8].copy_from_slice(&PACK_MAGIC);
    out[8..12].copy_from_slice(&unit.to_le_bytes());
    out
}

pub(crate) fn check_pack_header(bytes: &[u8], unit: u32) -> Result<(), &'static str> {
    if bytes.len() < PACK_HEADER as usize || bytes[..8] != PACK_MAGIC {
        return Err("it does not start as a pack of this store's format does");
    }
    if u32_at(bytes, 8) != unit || bytes[12..16].iter().any(|&it| it != 0) {
        return Err("its header names another unit than its head says");
    }
    Ok(())
}

/// The file of the pack at `at` in its store's head: a place the pack keeps, removed or not.
/// Packs sit next to the head, so the directory sync of the head's write makes a new pack's
/// name durable with it.
pub(crate) fn pack_file(at: usize) -> String {
    // alloc: a file name, which the effect naming it owns; once per file made or mapped
    format!("pack-{at:06}")
}

pub(crate) fn index_file(slot: u8) -> String {
    // alloc: a file name, which the effect naming it owns; once per file made or mapped
    format!("index-{slot}")
}

pub(crate) fn external_file(content: &ContentId) -> String {
    // alloc: a file name, which the effect naming it owns; once per file made or mapped
    format!("file-{content:x}")
}

pub(crate) const HEAD_FILE: &str = "head";

/// How a record holds its value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Form {
    /// The value's bytes, as they are: read in place.
    Raw,
    /// zstd with the dictionary of its kind numbered by [`RecordHeader::dictionary`], or, for a
    /// long value ([`crate::cold::LONG_FROM`]), with no dictionary ([`NO_DICTIONARY`]) and
    /// long-distance matching.
    Zstd,
    /// A list of chunks ([`CHUNK_REF`] bytes each), each a value of its own, raw or LZ4.
    Chunked,
    /// The value is a file of its own, [`external_file`] of its content: placed by cloning it.
    External,
    /// A compression dictionary, listed in the head; not a value.
    Dictionary,
    /// LZ4 (a block, no frame): for chunks, which are copied out anyway when their value is
    /// assembled, so decompressing them costs little more than the copy.
    Lz4,
    /// The value's columns bit-packed ([`crate::Columns::warm`]), read in place: the payload is
    /// the schema of the value ([`SCHEMA_PREFIX`] bytes), then the warm bytes.
    Warm,
    /// A codec per column ([`crate::Columns::cold`]), decoded when read: the payload is the
    /// schema of the value, then the cold bytes.
    ColdColumns,
}

/// What a [`Form::Warm`] or [`Form::ColdColumns`] payload starts with: the schema whose
/// encodings made it. A multiple of 16, so warm bytes keep the record's alignment.
pub(crate) const SCHEMA_PREFIX: usize = 32;

const _: () = assert!(SCHEMA_PREFIX.is_multiple_of(16) && SCHEMA_PREFIX == size_of::<blaze_format::SchemaHash>());

impl Form {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Form::Raw => 0,
            Form::Zstd => 1,
            Form::Chunked => 2,
            Form::External => 3,
            Form::Dictionary => 4,
            Form::Lz4 => 5,
            Form::Warm => 6,
            Form::ColdColumns => 7,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Form> {
        match code {
            0 => Some(Form::Raw),
            1 => Some(Form::Zstd),
            2 => Some(Form::Chunked),
            3 => Some(Form::External),
            4 => Some(Form::Dictionary),
            5 => Some(Form::Lz4),
            6 => Some(Form::Warm),
            7 => Some(Form::ColdColumns),
            _ => None,
        }
    }
}

pub(crate) const RECORD_HEADER: u64 = 96;
/// [`RecordHeader::dictionary`] of a value compressed without one.
pub(crate) const NO_DICTIONARY: u32 = u32::MAX;
const RECORD_MAGIC: [u8; 4] = *b"BLZR";
/// Where a record header keeps its CRC-32C, taken over the header with these bytes zero.
const RECORD_CHECK: std::ops::Range<usize> = 12..16;
/// A chunk in a chunked value's payload: its key and its length.
pub(crate) const CHUNK_REF: usize = 40;

/// What a record starts with. Its CRC-32C finds a torn append without reading the payload;
/// the payload is checked against `content` when it is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RecordHeader {
    pub kind: Kind,
    pub form: Form,
    pub dictionary: u32,
    /// Payload bytes after the header, padding not included.
    pub stored: u64,
    /// The value's length.
    pub raw: u64,
    pub key: Key,
    pub content: ContentId,
}

impl RecordHeader {
    pub(crate) fn encode(&self) -> [u8; RECORD_HEADER as usize] {
        let mut out = [0; RECORD_HEADER as usize];
        out[..4].copy_from_slice(&RECORD_MAGIC);
        out[4] = self.kind.code();
        out[5] = self.form.code();
        out[8..12].copy_from_slice(&self.dictionary.to_le_bytes());
        out[16..24].copy_from_slice(&self.stored.to_le_bytes());
        out[24..32].copy_from_slice(&self.raw.to_le_bytes());
        out[32..64].copy_from_slice(&self.key.0.0);
        out[64..96].copy_from_slice(&self.content.0);
        let check = crc::crc32c(&out);
        out[RECORD_CHECK].copy_from_slice(&check.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<RecordHeader, &'static str> {
        if bytes.len() < RECORD_HEADER as usize {
            return Err("its header is cut short");
        }
        let bytes = &bytes[..RECORD_HEADER as usize];
        if bytes[..4] != RECORD_MAGIC {
            return Err("its header has no record's magic");
        }
        let mut unchecked = [0; RECORD_HEADER as usize];
        unchecked.copy_from_slice(bytes);
        unchecked[RECORD_CHECK].fill(0);
        if u32_at(bytes, RECORD_CHECK.start) != crc::crc32c(&unchecked) {
            return Err("its header does not match its check");
        }
        let kind = Kind::from_code(bytes[4]).ok_or("its header names no kind")?;
        let form = Form::from_code(bytes[5]).ok_or("its header names no form")?;
        if span_of(u64_at(bytes, 16)).is_none() {
            return Err("its header says a length no record can have");
        }
        Ok(RecordHeader {
            kind,
            form,
            dictionary: u32_at(bytes, 8),
            stored: u64_at(bytes, 16),
            raw: u64_at(bytes, 24),
            key: Key(id_at(bytes, 32)),
            content: id_at(bytes, 64),
        })
    }

    /// The bytes the record takes in its pack, padding included.
    pub(crate) fn span(&self) -> u64 {
        span_of(self.stored).expect("a header is decoded, which refuses a length with no span, or made in memory")
    }
}

/// A whole record: header, payload, zeros to the next alignment.
pub(crate) fn record(header: &RecordHeader, payload: &[u8]) -> Vec<u8> {
    debug_assert_eq!(header.stored, payload.len() as u64);
    let mut out = Vec::with_capacity(header.span() as usize);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out.resize(header.span() as usize, 0);
    out
}

/// Appends one entry of a list of chunks to `out`: its key, then its length, `CHUNK_REF` bytes.
pub(crate) fn encode_chunk(out: &mut Vec<u8>, key: Key, len: u64) {
    out.put_bytes(&key.0.0);
    out.put_u64(len);
}

/// A list of chunks' entries, read in place as they are walked.
pub(crate) fn decode_chunks(
    payload: &[u8],
) -> Result<impl Iterator<Item = (Key, u64)> + Clone + '_, &'static str> {
    if payload.len() % CHUNK_REF != 0 {
        return Err("its list of chunks is not whole");
    }
    Ok(payload.chunks_exact(CHUNK_REF).map(|it| (Key(id_at(it, 0)), u64_at(it, 32))))
}

// Where records are.

const _: () = assert!(size_of::<Key>() == 32);
const _: () = assert!(size_of::<Loc>() == 32, "a place in the delta and the index is 32 bytes");

/// Where a record is, and enough of it to plan with: one slot of the index table, or an entry
/// of the in-memory delta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Loc {
    /// The pack's place in its store's head.
    pub pack: u32,
    pub kind: Kind,
    pub form: Form,
    pub offset: u64,
    pub stored: u64,
    pub raw: u64,
}

// The head.

const HEAD_MAGIC: [u8; 8] = *b"BLZHEAD\x04";
const HEAD_FIXED: usize = 8 + 4 + 4 + 8 + 8 + 8 + 4 + 4 + 8 + 4 + 4 + 4 + 4;
const PACK_REF: usize = 32;
const DICTIONARY_REF: usize = 16;
const EXTERNAL_REF: usize = 40;
const UNIT_REF: usize = 32;
const HEAD_CHECK: usize = 4;
const _: () = assert!(HEAD_FIXED == 72 && PACK_REF == 32);

/// The index table a head names: which of the two slots, its length and its CRC-32C.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct IndexRef {
    pub slot: u8,
    pub len: u64,
    pub check: u32,
    pub count: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PackRef {
    /// The crate version whose records it holds: 0 for what crates share, else one more than
    /// its place in [`Head::units`].
    pub unit: u32,
    /// Its bytes that are the store's: anything after is a torn append, cut before the next.
    pub committed: u64,
    /// How far the index table covers it; records from here to `committed` are found by
    /// reading their headers when the store opens.
    pub indexed: u64,
    /// The bytes of its records that another record of the same key has replaced (moved to
    /// another tier, or out of a pack being rewritten).
    pub dead: u64,
    /// Its live records were all copied on and the file is gone, or going: it is not mapped.
    /// It keeps its place in the list, which is the number index slots name.
    pub removed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DictionaryRef {
    pub kind: Kind,
    pub pack: u32,
    pub offset: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ExternalRef {
    pub content: ContentId,
    pub len: u64,
}

/// What a store is: written whole and at once (`Effect::Write`) after everything it names is
/// durable, so a crash leaves the old head or the new, each naming only whole bytes.
#[derive(Default, PartialEq, Eq, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: the tests tear copies of a head
pub(crate) struct Head {
    pub generation: u64,
    pub index: Option<IndexRef>,
    pub packs: Vec<PackRef>,
    pub dictionaries: Vec<DictionaryRef>,
    pub externals: Vec<ExternalRef>,
    /// The crate versions with packs of their own, each once and never zero; a pack's unit `n`
    /// is `units[n - 1]`.
    pub units: Vec<ContentId>,
}

impl Head {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            HEAD_FIXED
                + self.packs.len() * PACK_REF
                + self.dictionaries.len() * DICTIONARY_REF
                + self.externals.len() * EXTERNAL_REF
                + self.units.len() * UNIT_REF
                + HEAD_CHECK,
        );
        out.put_bytes(&HEAD_MAGIC);
        out.put_u32(crate::FORMAT);
        out.put_u32(0);
        out.put_u64(self.generation);
        let index = self.index.unwrap_or(IndexRef {
            slot: u8::MAX,
            len: 0,
            check: 0,
            count: 0,
        });
        out.put_u64(u64::from(index.slot));
        out.put_u64(index.len);
        out.put_u32(index.check);
        out.put_u32(0);
        out.put_u64(index.count);
        out.put_u32(self.packs.len() as u32);
        out.put_u32(self.dictionaries.len() as u32);
        out.put_u32(self.externals.len() as u32);
        out.put_u32(self.units.len() as u32);
        debug_assert_eq!(out.len(), HEAD_FIXED);
        for pack in &self.packs {
            out.put_u8(u8::from(pack.removed));
            out.put_bytes(&[0; 3]);
            out.put_u32(pack.unit);
            out.put_u64(pack.committed);
            out.put_u64(pack.indexed);
            out.put_u64(pack.dead);
        }
        for dictionary in &self.dictionaries {
            out.put_u8(dictionary.kind.code());
            out.put_bytes(&[0; 3]);
            out.put_u32(dictionary.pack);
            out.put_u64(dictionary.offset);
        }
        for external in &self.externals {
            out.put_bytes(&external.content.0);
            out.put_u64(external.len);
        }
        for unit in &self.units {
            out.put_bytes(&unit.0);
        }
        let check = crc::crc32c(&out);
        out.put_u32(check);
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Head, String> {
        if bytes.len() < HEAD_FIXED + HEAD_CHECK || bytes[..8] != HEAD_MAGIC {
            return Err("it is not the head of a store of this format".into());
        }
        let format = u32_at(bytes, 8);
        if format != crate::FORMAT {
            // alloc: the error's message
            return Err(format!(
                "it is of store format {format}, not {}: remove the directory to start a new \
                 store there",
                crate::FORMAT
            ));
        }
        let (body, check) = bytes.split_at(bytes.len() - HEAD_CHECK);
        if crc::crc32c(body).to_le_bytes() != check {
            return Err("it does not match its check".into());
        }
        let generation = u64_at(body, 16);
        let slot = u64_at(body, 24);
        let index = match slot {
            0 | 1 => Some(IndexRef {
                slot: slot as u8,
                len: u64_at(body, 32),
                check: u32_at(body, 40),
                count: u64_at(body, 48),
            }),
            s if s == u64::from(u8::MAX) => None,
            _ => return Err("it names an index slot there is not".into()),
        };
        let packs = u32_at(body, 56) as usize;
        let dictionaries = u32_at(body, 60) as usize;
        let externals = u32_at(body, 64) as usize;
        let units = u32_at(body, 68) as usize;
        let want = packs
            .checked_mul(PACK_REF)
            .zip(dictionaries.checked_mul(DICTIONARY_REF))
            .zip(externals.checked_mul(EXTERNAL_REF))
            .zip(units.checked_mul(UNIT_REF))
            .and_then(|(((a, b), c), d)| {
                a.checked_add(b)?.checked_add(c)?.checked_add(d)?.checked_add(HEAD_FIXED)
            });
        if want != Some(body.len()) {
            return Err("its length is not what its counts say".into());
        }
        let mut read = Reader::starting_at(body, HEAD_FIXED)?;
        let mut head = Head { generation, index, ..Head::default() };
        for _ in 0..packs {
            let removed = match read.u8()? {
                0 => false,
                1 => true,
                _ => return Err("a pack neither kept nor removed".into()),
            };
            if *read.array::<3>()? != [0; 3] {
                return Err("a pack has bytes set that no head sets".into());
            }
            let unit = read.u32()?;
            let (committed, indexed, dead) = (read.u64()?, read.u64()?, read.u64()?);
            head.packs.push(PackRef { unit, committed, indexed, dead, removed });
        }
        for _ in 0..dictionaries {
            let kind = Kind::from_code(read.u8()?).ok_or("a dictionary of no kind")?;
            read.skip(3)?;
            let pack = read.u32()?;
            if pack as usize >= head.packs.len() {
                return Err("a dictionary in a pack it does not have".into());
            }
            head.dictionaries.push(DictionaryRef { kind, pack, offset: read.u64()? });
        }
        for _ in 0..externals {
            let content = ContentId(*read.array()?);
            head.externals.push(ExternalRef { content, len: read.u64()? });
        }
        let mut seen = blaze_hash::HashSet::default();
        for _ in 0..units {
            let unit = ContentId(*read.array()?);
            if unit == ContentId::default() || !seen.insert(unit) {
                return Err("it lists a unit twice, or one of no crate".into());
            }
            head.units.push(unit);
        }
        for (number, pack) in head.packs.iter().enumerate() {
            if pack.indexed > pack.committed
                || pack.dead > pack.committed
                || pack.indexed % ALIGN != 0
                || pack.dead % ALIGN != 0
                || pack.committed < PACK_HEADER
                || pack.committed % BLOCK != 0
            {
                // alloc: the error's message
                return Err(format!("{} has impossible lengths", pack_file(number)));
            }
            if pack.unit as usize > head.units.len() {
                // alloc: the error's message
                return Err(format!("{} is of a unit it does not list", pack_file(number)));
            }
        }
        Ok(head)
    }
}
