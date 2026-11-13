//! A pack's manifest: the records it holds, and under which pin they are kept.
//!
//! Its bytes, deterministic CBOR ([`crate::cbor`]), one array:
//!
//! ```text
//! [ "blaze pack manifest", 1, coding, pin, scope, [ [key, kind, len, content], ... ] ]
//! ```
//!
//! - `coding` is how the body holds each record: 0, as the store gave it. (Compressed
//!   codings are numbers to come; a reader refuses one it does not know, by its number.)
//! - `pin` is the name the importing store pins the records under, so its collector keeps
//!   them from the import on (`toolchain`, `bstd`, any name a build was kept under); `scope`
//!   says which toolchain and target the pack was made for. Both are labels to find a pack
//!   by, never a part of a key: a pack of another scope is records whose keys no build asks.
//! - Records are sorted by key, each key once: two exports of one selection are the same
//!   bytes, and a reader refuses any other order, so a manifest has one encoding.
//! - The body is the records' bytes in that order with nothing between them: a record's
//!   place is the sum of the lengths before it ([`Manifest::at`]), which the reader computes
//!   and no manifest states, so no stated offset can point outside the body or overlap.
//!
//! The pack's name is the hash of these bytes ([`crate::Hashes::manifest`]).
//!
//! Bounds, each checked before anything is allocated by it: the bytes
//! ([`MANIFEST_MOST`]), the records ([`RECORDS_MOST`]), a record ([`RECORD_MOST`]), the body
//! ([`BODY_MOST`]).

use alloc::{
    format,
    string::{String, ToString as _},
    vec::Vec,
};

use crate::{Id, Refused, cbor, layout};

const MAGIC: &str = "blaze pack manifest";
/// The manifest's layout. A manifest of another is refused by its number, never read as this.
pub const LAYOUT: u64 = 1;
/// The body's coding this blaze writes and reads: each record as the store gave it.
pub const RAW: u64 = 0;

/// The most bytes of a manifest.
pub const MANIFEST_MOST: u64 = 128 << 20;
/// The most records of one pack.
pub const RECORDS_MOST: u64 = 1 << 20;
/// The most bytes of one record: the store's own bound on a value that travels.
pub const RECORD_MOST: u64 = 1 << 30;
/// The most bytes of one pack's body.
pub const BODY_MOST: u64 = 1 << 40;

const WHAT: &str = "the pack's manifest";

/// One record of a pack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    /// The store key the record is kept under.
    pub key: Id,
    /// The store's code of its kind.
    pub kind: u8,
    pub len: u64,
    /// The store's content hash of its bytes.
    pub content: Id,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Manifest {
    pin: String,
    scope: String,
    records: Vec<Record>,
    /// Where each record starts in the body, and after the last, the body's length.
    starts: Vec<u64>,
}

impl Manifest {
    /// The manifest of `records` under `pin` for `scope`: sorted by key, a record given twice
    /// kept once. Two records of one key that differ are refused naming the key, as are a
    /// name no path may hold and anything past a bound.
    pub fn new(pin: &str, scope: &str, mut records: Vec<Record>) -> Result<Manifest, Refused> {
        layout::name("the pin", pin)?;
        layout::name("the scope", scope)?;
        records.sort_unstable_by(|a, b| a.key.cmp(&b.key).then(a.content.cmp(&b.content)));
        for pair in records.windows(2) {
            if pair[0].key == pair[1].key && pair[0] != pair[1] {
                return Err(Refused::Differs {
                    key: pair[0].key,
                    kept: pair[0].content,
                    given: pair[1].content,
                });
            }
        }
        records.dedup();
        let starts = starts(&records)?;
        // alloc: the manifest's own names, once a manifest
        Ok(Manifest { pin: pin.to_string(), scope: scope.to_string(), records, starts })
    }

    pub fn pin(&self) -> &str {
        &self.pin
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Where record `index` starts in the body.
    pub fn at(&self, index: u32) -> u64 {
        self.starts[index as usize]
    }

    /// The body's length.
    pub fn body_len(&self) -> u64 {
        self.starts[self.records.len()]
    }

    /// The index of the record of `key`.
    pub fn find(&self, key: &Id) -> Option<u32> {
        self.records.binary_search_by(|it| it.key.cmp(key)).ok().map(|at| at as u32)
    }

    pub fn encode(&self) -> Vec<u8> {
        // alloc: the manifest's bytes, which are the output
        let mut out = Vec::with_capacity(64 + self.records.len() * 80);
        cbor::array(&mut out, 6);
        cbor::text(&mut out, MAGIC);
        cbor::uint(&mut out, LAYOUT);
        cbor::uint(&mut out, RAW);
        cbor::text(&mut out, &self.pin);
        cbor::text(&mut out, &self.scope);
        cbor::array(&mut out, self.records.len() as u64);
        for record in &self.records {
            cbor::array(&mut out, 4);
            cbor::bytes(&mut out, &record.key.0);
            cbor::uint(&mut out, u64::from(record.kind));
            cbor::uint(&mut out, record.len);
            cbor::bytes(&mut out, &record.content.0);
        }
        out
    }

    /// The manifest `bytes` are, or why they are none. Checks nothing against a pack's name:
    /// that is [`Manifest::named`]'s.
    pub fn decode(bytes: &[u8]) -> Result<Manifest, Refused> {
        if bytes.len() as u64 > MANIFEST_MOST {
            return Err(Refused::TooLarge { what: WHAT, len: bytes.len() as u64, most: MANIFEST_MOST });
        }
        let mut reader = cbor::Reader::new(bytes);
        let malformed = |error: cbor::Error| Refused::Malformed { what: WHAT, at: error.at, why: error.why };
        let foreign = || Refused::Layout { what: WHAT, found: "not a blaze pack manifest".to_string() };
        // What is not this format at all is said as that, before any detail of it.
        if reader.array().map_err(|_| foreign())? != 6 {
            return Err(foreign());
        }
        if reader.text().map_err(|_| foreign())? != MAGIC {
            return Err(foreign());
        }
        let version = reader.uint().map_err(malformed)?;
        if version != LAYOUT {
            // alloc: an error's text
            return Err(Refused::Layout { what: WHAT, found: format!("of layout {version}, not {LAYOUT}") });
        }
        let coding = reader.uint().map_err(malformed)?;
        if coding != RAW {
            // alloc: an error's text
            return Err(Refused::Layout { what: WHAT, found: format!("of body coding {coding}, not {RAW}") });
        }
        let pin = layout::name("the pin", reader.text().map_err(malformed)?)?;
        let scope = layout::name("the scope", reader.text().map_err(malformed)?)?;
        let count = reader.array().map_err(malformed)?;
        if count > RECORDS_MOST {
            return Err(Refused::TooLarge { what: "the pack's record count", len: count, most: RECORDS_MOST });
        }
        // alloc: the records, at most `RECORDS_MOST`, and no more than the input has bytes
        let mut records: Vec<Record> = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let at = reader.at();
            reader.array_of(4).map_err(malformed)?;
            let key = Id(reader.fixed::<32>().map_err(malformed)?);
            let kind = reader.uint().map_err(malformed)?;
            let Ok(kind) = u8::try_from(kind) else {
                return Err(Refused::Malformed { what: WHAT, at, why: "a record's kind is not a byte" });
            };
            let len = reader.uint().map_err(malformed)?;
            let content = Id(reader.fixed::<32>().map_err(malformed)?);
            if records.last().is_some_and(|last| last.key >= key) {
                return Err(Refused::Malformed {
                    what: WHAT,
                    at,
                    why: "its records are not sorted by key, each key once",
                });
            }
            records.push(Record { key, kind, len, content });
        }
        reader.end().map_err(malformed)?;
        let starts = starts(&records)?;
        // alloc: the manifest's own names, once a manifest
        Ok(Manifest { pin: pin.to_string(), scope: scope.to_string(), records, starts })
    }

    /// [`Manifest::decode`] of the bytes of the pack `pack`: their hash first, so nothing of
    /// bytes that are not the pack's is read.
    pub fn named(pack: &Id, bytes: &[u8], hashes: &impl crate::Hashes) -> Result<Manifest, Refused> {
        if bytes.len() as u64 > MANIFEST_MOST {
            return Err(Refused::TooLarge { what: WHAT, len: bytes.len() as u64, most: MANIFEST_MOST });
        }
        let got = hashes.manifest(bytes);
        if got != *pack {
            return Err(Refused::Manifest { want: *pack, got });
        }
        Manifest::decode(bytes)
    }
}

/// Where each record starts; a record or a body past its bound is refused naming it.
fn starts(records: &[Record]) -> Result<Vec<u64>, Refused> {
    if records.len() as u64 > RECORDS_MOST {
        return Err(Refused::TooLarge {
            what: "the pack's record count",
            len: records.len() as u64,
            most: RECORDS_MOST,
        });
    }
    // alloc: once a manifest, a word a record
    let mut starts = Vec::with_capacity(records.len() + 1);
    let mut at = 0u64;
    for record in records {
        if record.len > RECORD_MOST {
            return Err(Refused::TooLarge { what: "a record of the pack", len: record.len, most: RECORD_MOST });
        }
        starts.push(at);
        // At most 2^20 records of at most 2^30 bytes: no overflow of a `u64`.
        at += record.len;
        if at > BODY_MOST {
            return Err(Refused::TooLarge { what: "the pack's body", len: at, most: BODY_MOST });
        }
    }
    starts.push(at);
    Ok(starts)
}
