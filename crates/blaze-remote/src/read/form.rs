//! The objects an origin holds for the read path (remote-read.md, "Objects"), their encoders and
//! their decoders. Every decoder checks its object's length against a limit before it reads a
//! field, and a digest or a signature before it trusts one. Nothing here does IO.
//!
//! Little-endian. Names are built only from a digest's hex ([`Object::name`]): no byte read from
//! an origin ever becomes part of a name, so no name can leave its namespace.
//!
//! - `head`, [`HEAD_LEN`] bytes: `BLRH`, format u32, namespace u64, generation u64, the
//!   manifest's digest (32) and length u64, the signer's public key (32), then the signature
//!   (64) over bytes `0..96` under [`HEAD_DOMAIN`].
//! - `manifest-<digest>`: `BLRM`, format u32, namespace u64, rows u32, zero u32, generation
//!   u64, then rows of [`ROW`] bytes, sorted by segment digest, each segment once: segment
//!   digest 32, segment length u64, index offset u64, records u32, pages u32, entries a page
//!   u32, zero u32, summary digest 32, summary length u64. Its digest is the head's.
//! - `seg-<digest>`: the plain store's segment, byte for byte (blaze-cas `seal_segment`): the
//!   payloads are read from it by range, in place.
//! - `sum-<segment digest>`: `BLRF`, format u32, segment digest 32, records u32, pages u32,
//!   entries a page u32, filter length u32, zero u64; the segment's [`Fuse8`] over its keys'
//!   first eight bytes; zeros to 8; then per index page its first key (32) and its digest (32).
//!   Its digest is the manifest row's.
//! - `idx-<segment digest>`: the segment's index entries, each the store's own 48 bytes
//!   (blaze-cas `Entry`) and the value's digest (32, [`crate::segment::value_digest`]), in key
//!   order, read a page at a time. A page's digest is in the summary.
//! - `prefix`: the head's bytes, then the manifest's, then each row's summary in the manifest's
//!   row order (published.md): nothing of its own, so each part passes its own check above, and
//!   its length is the sum the head and the rows say ([`decode_prefix`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::fmt;

use blaze_cas::plain::{Entry, Namespace};
use blaze_ids::ContentId;
use blaze_ids::fuse::Fuse8;
use blaze_keyed::store::{ENTRY, FOOTER, HEADER, SEGMENT_MOST};
use blaze_own::{Alloc, Vec};

use crate::trust::PublicKey;
use crate::wire::{id_at, key_at, sig_at, u32_at, u64_at};

/// The read path's own objects' format.
pub const FORMAT: u32 = 1;
pub const HEAD_LEN: usize = 160;
/// The bytes of a head its signature covers.
pub const HEAD_SIGNED: usize = 96;
pub const HEAD_DOMAIN: &str = "blaze-remote origin head 1";
pub const MANIFEST_DOMAIN: &str = "blaze-remote origin manifest 1";
pub const SUMMARY_DOMAIN: &str = "blaze-remote origin summary 1";
pub const PAGE_DOMAIN: &str = "blaze-remote origin index page 1";
const HEAD_MAGIC: [u8; 4] = *b"BLRH";
const MANIFEST_MAGIC: [u8; 4] = *b"BLRM";
const SUMMARY_MAGIC: [u8; 4] = *b"BLRF";
pub const MANIFEST_HEADER: usize = 32;
pub const ROW: usize = 104;
/// Segments one manifest names at most: a manifest is then at most 416 KiB.
pub const ROWS_MOST: u32 = 4096;
pub const MANIFEST_MOST: u64 = (MANIFEST_HEADER + ROW * ROWS_MOST as usize) as u64;
pub const SUMMARY_HEADER: usize = 64;
/// A page's first key and digest in a summary.
pub const FENCE: usize = 64;
/// A summary's bytes at most: a segment of about 4 million records.
pub const SUMMARY_MOST: u64 = 8 << 20;
/// One entry of an `idx-` object: the store's entry and the value's digest.
pub const REMOTE_ENTRY: usize = ENTRY + 32;
/// Entries an index page holds at most, so a page is at most 64 KiB: one ranged read.
pub const PAGE_ENTRIES_MOST: u32 = ((64 << 10) / REMOTE_ENTRY) as u32;
/// Records a segment holds at most, as a manifest row may say.
pub const COUNT_MOST: u32 = 1 << 26;
/// A prefix's bytes at most, whatever a reader's `Limits::prefix_most` says: the head, a
/// manifest and the summaries of a published set of some millions of records.
pub const PREFIX_MOST: u64 = 16 << 20;

/// An object of an origin, by what it is. Its name is spelt from a digest's hex alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Object {
    Head,
    /// The head, the manifest and every summary in one object (published.md, "The origin's
    /// layout"): what a fresh reader asks first, in one read.
    Prefix,
    Manifest(ContentId),
    Segment(ContentId),
    Summary(ContentId),
    Index(ContentId),
}

/// An object's name: `head`, or a prefix and 64 lowercase hex digits. At most 73 bytes, every one
/// of `[a-z0-9-]`, so it never holds a `/`, a `.` or anything else a path could be made of.
#[derive(Clone, Copy)]
pub struct Name {
    bytes: [u8; 73],
    len: u8,
}

impl Name {
    pub fn as_str(&self) -> &str {
        // Every byte was written from ASCII below.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("an object's name is ASCII")
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Object {
    pub fn name(&self) -> Name {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0u8; 73];
        let (prefix, digest): (&[u8], Option<&ContentId>) = match self {
            Object::Head => (b"head", None),
            Object::Prefix => (b"prefix", None),
            Object::Manifest(id) => (b"manifest-", Some(id)),
            Object::Segment(id) => (b"seg-", Some(id)),
            Object::Summary(id) => (b"sum-", Some(id)),
            Object::Index(id) => (b"idx-", Some(id)),
        };
        bytes[..prefix.len()].copy_from_slice(prefix);
        let mut len = prefix.len();
        if let Some(id) = digest {
            for byte in id.0 {
                bytes[len] = HEX[usize::from(byte >> 4)];
                bytes[len + 1] = HEX[usize::from(byte & 15)];
                len += 2;
            }
        }
        Name { bytes, len: len as u8 }
    }
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A digest's first 12 hex digits name it in a message, as the store's reports do.
        let name = self.name();
        let text = name.as_str();
        f.write_str(&text[..text.len().min(match self {
            Object::Head => 4,
            Object::Prefix => 6,
            Object::Manifest(_) => 9 + 12,
            Object::Segment(_) | Object::Summary(_) | Object::Index(_) => 4 + 12,
        })])
    }
}

/// Why a fetched object was refused: each names the object (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Its bytes are not the ones its digest (from the object that names it) says.
    Digest { object: Object },
    /// Not of this form: what is wrong, and at which byte.
    Malformed { object: Object, what: &'static str, at: u64 },
    /// Of another format.
    Format { object: Object, found: u32, wanted: u32 },
    /// A length it says is past what this reader takes, refused before anything is allocated.
    TooLarge { object: Object, what: &'static str, len: u64, most: u64 },
    /// Of another namespace than the one asked.
    Namespace { found: Namespace, wanted: Namespace },
    /// The head's signature does not stand.
    Signature { signer: PublicKey, why: SignatureWhy },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Digest { object } => write!(f, "{object}: its bytes are not the ones its digest names (torn, cut or forged)"),
            Refused::Malformed { object, what, at } => write!(f, "{object} at byte {at}: {what}"),
            Refused::Format { object, found, wanted } => write!(f, "{object}: format {found}, this blaze reads {wanted}"),
            Refused::TooLarge { object, what, len, most } => write!(f, "{object}: {what} is {len}, past the {most} this reader takes"),
            Refused::Namespace { found, wanted } => write!(f, "head: of namespace {:#x}, {:#x} was asked", found.0, wanted.0),
            Refused::Signature { signer, why } => {
                write!(f, "head: signed by {}: {why}", blaze_format::hex::encode(&signer.0[..6]))
            }
        }
    }
}

/// Why a signature does not stand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignatureWhy {
    /// The signer is not a key this reader was given for the namespace.
    UnknownSigner,
    /// The signature is not the signer's over the head: blaze-ed25519's reason.
    Invalid(blaze_ed25519::Refused),
    /// The signature stands, but the head's generation is below one this reader has taken for
    /// its set (a rollback to an older published set: blaze-remote published/set.rs).
    Stale { generation: u64, seen: u64 },
}

impl fmt::Display for SignatureWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignatureWhy::UnknownSigner => f.write_str("a key this blaze was not given for the namespace"),
            SignatureWhy::Invalid(why) => write!(f, "the signature is not its signer's over the head: {why}"),
            SignatureWhy::Stale { generation, seen } => {
                write!(f, "its generation {generation} is below the {seen} this home has taken for the set: an older set served again (a rollback), not read")
            }
        }
    }
}

/// The check of a head's signature: the one seam where a signature primitive plugs in
/// (remote-read.md, "The gap"). `Ok` only when `signer` is a key given for `namespace` and
/// `signature` is its signature over `domain || 0 || message`.
pub trait HeadCheck {
    fn check(&self, namespace: Namespace, signer: &PublicKey, domain: &'static str, message: &[u8], signature: &[u8; 64]) -> Result<(), SignatureWhy>;
}

/// Ed25519 over the keys given for one namespace, by blaze-ed25519 (remote-read.md, "Ed25519,
/// written here"): RFC 8032's strict check, no third-party code, in every shape of this crate.
pub struct Ed25519Keys<'k> {
    pub namespace: Namespace,
    pub keys: &'k [PublicKey],
}

impl HeadCheck for Ed25519Keys<'_> {
    fn check(&self, namespace: Namespace, signer: &PublicKey, domain: &'static str, message: &[u8], signature: &[u8; 64]) -> Result<(), SignatureWhy> {
        if namespace != self.namespace || !self.keys.contains(signer) {
            return Err(SignatureWhy::UnknownSigner);
        }
        if let Err(why) = crate::trust::verify_sig(signer, domain, message, signature) {
            return Err(SignatureWhy::Invalid(why));
        }
        Ok(())
    }
}

/// A namespace's head: which manifest is current.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Head {
    pub namespace: Namespace,
    pub generation: u64,
    pub manifest: ContentId,
    pub manifest_len: u64,
    pub signer: PublicKey,
}

impl Head {
    /// Its bytes, signed by `sign` (given `domain` and the signed bytes).
    pub fn encode(&self, sign: impl FnOnce(&'static str, &[u8]) -> [u8; 64]) -> [u8; HEAD_LEN] {
        let mut out = [0u8; HEAD_LEN];
        out[0..4].copy_from_slice(&HEAD_MAGIC);
        out[4..8].copy_from_slice(&FORMAT.to_le_bytes());
        out[8..16].copy_from_slice(&self.namespace.0.to_le_bytes());
        out[16..24].copy_from_slice(&self.generation.to_le_bytes());
        out[24..56].copy_from_slice(&self.manifest.0);
        out[56..64].copy_from_slice(&self.manifest_len.to_le_bytes());
        out[64..96].copy_from_slice(&self.signer.0);
        let signature = sign(HEAD_DOMAIN, &out[..HEAD_SIGNED]);
        out[96..160].copy_from_slice(&signature);
        out
    }

    /// The head `bytes` are, its signature checked by `check` before any field is believed.
    pub fn decode(bytes: &[u8], namespace: Namespace, check: &dyn HeadCheck) -> Result<Head, Refused> {
        let object = Object::Head;
        if bytes.len() != HEAD_LEN {
            return Err(Refused::Malformed { object, what: "a head is 160 bytes", at: bytes.len() as u64 });
        }
        if bytes[0..4] != HEAD_MAGIC {
            return Err(Refused::Malformed { object, what: "does not start with `BLRH`", at: 0 });
        }
        if u32_at(bytes, 4) != FORMAT {
            return Err(Refused::Format { object, found: u32_at(bytes, 4), wanted: FORMAT });
        }
        let found = Namespace(u64_at(bytes, 8));
        if found != namespace {
            return Err(Refused::Namespace { found, wanted: namespace });
        }
        let signer = PublicKey(key_at(bytes, 64));
        if !crate::planted(crate::Plant::HeadUnsigned) {
            check.check(namespace, &signer, HEAD_DOMAIN, &bytes[..HEAD_SIGNED], &sig_at(bytes, 96)).map_err(|why| Refused::Signature { signer, why })?;
        }
        let manifest_len = u64_at(bytes, 56);
        if !(MANIFEST_HEADER as u64..=MANIFEST_MOST).contains(&manifest_len) {
            return Err(Refused::TooLarge { object, what: "the manifest's length", len: manifest_len, most: MANIFEST_MOST });
        }
        Ok(Head { namespace, generation: u64_at(bytes, 16), manifest: id_at(bytes, 24), manifest_len, signer })
    }
}

/// One segment as a manifest names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub segment: ContentId,
    pub len: u64,
    pub index_at: u64,
    pub count: u32,
    pub pages: u32,
    pub page_entries: u32,
    pub summary: ContentId,
    pub summary_len: u64,
}

impl Row {
    /// The entries of page `page`.
    pub fn page_len(&self, page: u32) -> u32 {
        let first = u64::from(page) * u64::from(self.page_entries);
        (u64::from(self.count) - first).min(u64::from(self.page_entries)) as u32
    }

    fn encode_into(&self, out: &mut Vec<u8, Alloc<'_>>) {
        out.extend_from_slice(&self.segment.0);
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.index_at.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.pages.to_le_bytes());
        out.extend_from_slice(&self.page_entries.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.summary.0);
        out.extend_from_slice(&self.summary_len.to_le_bytes());
    }

    /// The row at `bytes[..ROW]`, every field held to what a sealed segment can be.
    fn decode(object: Object, bytes: &[u8], at: u64) -> Result<Row, Refused> {
        let bad = |what| Refused::Malformed { object, what, at };
        let row = Row {
            segment: id_at(bytes, 0),
            len: u64_at(bytes, 32),
            index_at: u64_at(bytes, 40),
            count: u32_at(bytes, 48),
            pages: u32_at(bytes, 52),
            page_entries: u32_at(bytes, 56),
            summary: id_at(bytes, 64),
            summary_len: u64_at(bytes, 96),
        };
        if u32_at(bytes, 60) != 0 {
            return Err(bad("a row's reserved word is not zero"));
        }
        if row.len > SEGMENT_MOST || row.len < (HEADER + FOOTER) as u64 {
            return Err(Refused::TooLarge { object, what: "a segment's length", len: row.len, most: SEGMENT_MOST });
        }
        if row.count > COUNT_MOST {
            return Err(Refused::TooLarge { object, what: "a segment's records", len: u64::from(row.count), most: u64::from(COUNT_MOST) });
        }
        let end = row.index_at.checked_add(u64::from(row.count) * ENTRY as u64).and_then(|end| end.checked_add(FOOTER as u64));
        if row.index_at < HEADER as u64 || end != Some(row.len) {
            return Err(bad("a row's index does not lie after the header and end at the footer"));
        }
        if !(1..=PAGE_ENTRIES_MOST).contains(&row.page_entries) {
            return Err(Refused::TooLarge { object, what: "a page's entries", len: u64::from(row.page_entries), most: u64::from(PAGE_ENTRIES_MOST) });
        }
        if row.pages != row.count.div_ceil(row.page_entries) {
            return Err(bad("a row's pages are not its records over its entries a page"));
        }
        let least = (SUMMARY_HEADER + FENCE * row.pages as usize) as u64;
        if row.summary_len > SUMMARY_MOST || row.summary_len < least {
            return Err(Refused::TooLarge { object, what: "a summary's length", len: row.summary_len, most: SUMMARY_MOST });
        }
        Ok(row)
    }
}

/// A manifest's bytes: `rows` sorted by segment, each once.
pub fn encode_manifest(namespace: Namespace, generation: u64, rows: &[Row], out: &mut Vec<u8, Alloc<'_>>) {
    out.clear();
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&namespace.0.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    for row in rows {
        row.encode_into(out);
    }
}

/// The rows of the manifest `head` names, its bytes checked against the head's digest first.
pub fn decode_manifest(bytes: &[u8], head: &Head, into: &mut Vec<Row, Alloc<'_>>) -> Result<(), Refused> {
    let object = Object::Manifest(head.manifest);
    if bytes.len() as u64 != head.manifest_len {
        return Err(Refused::Malformed { object, what: "not the length its head says", at: bytes.len() as u64 });
    }
    if ContentId::of(MANIFEST_DOMAIN, bytes) != head.manifest {
        return Err(Refused::Digest { object });
    }
    if bytes[0..4] != MANIFEST_MAGIC {
        return Err(Refused::Malformed { object, what: "does not start with `BLRM`", at: 0 });
    }
    if u32_at(bytes, 4) != FORMAT {
        return Err(Refused::Format { object, found: u32_at(bytes, 4), wanted: FORMAT });
    }
    if Namespace(u64_at(bytes, 8)) != head.namespace {
        return Err(Refused::Namespace { found: Namespace(u64_at(bytes, 8)), wanted: head.namespace });
    }
    let rows = u32_at(bytes, 16);
    if rows > ROWS_MOST {
        return Err(Refused::TooLarge { object, what: "its rows", len: u64::from(rows), most: u64::from(ROWS_MOST) });
    }
    if bytes.len() != MANIFEST_HEADER + ROW * rows as usize || u32_at(bytes, 20) != 0 {
        return Err(Refused::Malformed { object, what: "its length is not its rows'", at: 16 });
    }
    into.clear();
    into.reserve(rows as usize);
    for i in 0..rows as usize {
        let at = MANIFEST_HEADER + i * ROW;
        let row = Row::decode(object, &bytes[at..at + ROW], at as u64)?;
        if let Some(last) = into.as_slice().last()
            && last.segment >= row.segment
        {
            return Err(Refused::Malformed { object, what: "its rows are not sorted by segment, each once", at: at as u64 });
        }
        into.push(row);
    }
    Ok(())
}

/// What the read path keeps of a segment's summary: its filter and its pages' fences.
pub struct Summary<'a> {
    /// `None` for a segment of no records: it holds nothing.
    filter: Option<Fuse8>,
    /// Per page: its first key, its digest.
    fences: Vec<u8, Alloc<'a>>,
}

/// The first eight bytes of a key, as the filter holds it.
pub fn filter_word(key: &ContentId) -> u64 {
    u64::from_le_bytes(key.0[0..8].try_into().expect("8 bytes"))
}

/// A summary's bytes for the segment `segment` of `entries` (each its key), cut in pages of
/// `page_entries`, whose page digests are `pages`.
pub fn encode_summary(segment: ContentId, keys: &[ContentId], page_entries: u32, pages: &[ContentId], out: &mut Vec<u8, Alloc<'_>>) {
    out.clear();
    let mut filter: std::vec::Vec<u8> = std::vec::Vec::new();
    if !keys.is_empty() {
        // alloc: Fuse8 takes and builds in std's heap, once per segment sealed
        Fuse8::of(keys.iter().map(filter_word).collect()).encode(&mut filter);
    }
    out.extend_from_slice(&SUMMARY_MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&segment.0);
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    out.extend_from_slice(&page_entries.to_le_bytes());
    out.extend_from_slice(&(filter.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&filter);
    out.resize(out.len().next_multiple_of(8), 0);
    for (page, digest) in pages.iter().enumerate() {
        out.extend_from_slice(&keys[page * page_entries as usize].0);
        out.extend_from_slice(&digest.0);
    }
}

impl<'a> Summary<'a> {
    /// The summary of `row`'s segment, its bytes checked against the row's digest first.
    pub fn decode(bytes: &[u8], row: &Row, alloc: Alloc<'a>) -> Result<Summary<'a>, Refused> {
        let object = Object::Summary(row.segment);
        let bad = |what, at| Refused::Malformed { object, what, at };
        if bytes.len() as u64 != row.summary_len {
            return Err(bad("not the length its manifest row says", bytes.len() as u64));
        }
        if ContentId::of(SUMMARY_DOMAIN, bytes) != row.summary {
            return Err(Refused::Digest { object });
        }
        if bytes[0..4] != SUMMARY_MAGIC {
            return Err(bad("does not start with `BLRF`", 0));
        }
        if u32_at(bytes, 4) != FORMAT {
            return Err(Refused::Format { object, found: u32_at(bytes, 4), wanted: FORMAT });
        }
        if id_at(bytes, 8) != row.segment || u32_at(bytes, 40) != row.count || u32_at(bytes, 44) != row.pages || u32_at(bytes, 48) != row.page_entries {
            return Err(bad("does not describe the segment its row names", 8));
        }
        let filter_len = u64::from(u32_at(bytes, 52));
        let fences_at = (SUMMARY_HEADER as u64 + filter_len).next_multiple_of(8);
        if fences_at + (FENCE as u64) * u64::from(row.pages) != bytes.len() as u64 {
            return Err(bad("its filter and fences are not its length", 52));
        }
        let filter = match (row.count, filter_len) {
            (0, 0) => None,
            (0, _) | (_, 0) => return Err(bad("a filter for no records, or none for some", 52)),
            _ => Some(Fuse8::decode(&bytes[SUMMARY_HEADER..SUMMARY_HEADER + filter_len as usize]).map_err(|what| bad(what, SUMMARY_HEADER as u64))?),
        };
        let fences = &bytes[fences_at as usize..];
        for page in 1..row.pages as usize {
            if fences[(page - 1) * FENCE..(page - 1) * FENCE + 32] >= fences[page * FENCE..page * FENCE + 32] {
                return Err(bad("its pages' first keys are not ascending", fences_at + (page * FENCE) as u64));
            }
        }
        let mut held = Vec::with_capacity_in(fences.len(), alloc);
        held.extend_from_slice(fences);
        Ok(Summary { filter, fences: held })
    }

    /// The page of the segment that holds `key` if any does: `None` when the filter or the
    /// first page's first key says it is not there (no request needed).
    pub fn page_of(&self, key: &ContentId) -> Option<u32> {
        let filter = self.filter.as_ref()?;
        if !filter.contains(filter_word(key)) {
            return None;
        }
        let pages = self.fences.len() / FENCE;
        // The last page whose first key is at most `key`.
        let (mut lo, mut hi) = (0usize, pages);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.fences[mid * FENCE..mid * FENCE + 32] <= key.0[..] {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo > 0).then(|| (lo - 1) as u32)
    }

    pub fn page_digest(&self, page: u32) -> ContentId {
        id_at(&self.fences, page as usize * FENCE + 32)
    }

    /// Whether `key` passes the filter: what a test of the filter asks.
    pub fn may_hold(&self, key: &ContentId) -> bool {
        self.filter.as_ref().is_some_and(|filter| filter.contains(filter_word(key)))
    }
}

/// A page's digest.
pub fn page_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(PAGE_DOMAIN, bytes)
}

/// The record `key` in the checked index page `page` of `row`: its entry and its value's
/// digest, every place it says held inside the segment's payloads. `Ok(None)` when the page
/// does not hold it.
pub fn lookup(page: &[u8], row: &Row, page_no: u32, key: &ContentId) -> Result<Option<(Entry, ContentId)>, Refused> {
    let object = Object::Index(row.segment);
    let n = page.len() / REMOTE_ENTRY;
    let found = blaze_cas::plain::find_many(n, core::slice::from_ref(key), |i| &page[i * REMOTE_ENTRY..i * REMOTE_ENTRY + 32]);
    let Some(i) = found[0] else { return Ok(None) };
    let at = (u64::from(page_no) * u64::from(row.page_entries) + i as u64) * REMOTE_ENTRY as u64;
    let bytes = &page[i * REMOTE_ENTRY..(i + 1) * REMOTE_ENTRY];
    let entry = Entry::decode(&bytes[..ENTRY]).map_err(|what| Refused::Malformed { object, what, at })?;
    if entry.at < HEADER as u64 || entry.at.checked_add(u64::from(entry.len)).is_none_or(|end| end > row.index_at) {
        return Err(Refused::Malformed { object, what: "an entry places its payload outside the segment's payloads", at });
    }
    Ok(Some((entry, id_at(bytes, ENTRY))))
}

/// What a prefix holds, every part checked: the head, the manifest's rows, and each row's
/// summary in the rows' order.
pub struct Prefixed<'a> {
    pub head: Head,
    pub rows: Vec<Row, Alloc<'a>>,
    pub summaries: Vec<Summary<'a>, Alloc<'a>>,
}

/// The prefix `bytes` are, read by a reader that takes at most `most` bytes of one (and never
/// more than [`PREFIX_MOST`]). The head's signature is checked before any other field is
/// believed, the manifest against the head's digest, each summary against its row; the
/// prefix's length must be exactly the sum the head and the rows say, so a cut or a padded one
/// is refused, and a declared length past `most` is refused before a summary is read.
pub fn decode_prefix<'a>(bytes: &[u8], most: u64, namespace: Namespace, check: &dyn HeadCheck, alloc: Alloc<'a>) -> Result<Prefixed<'a>, Refused> {
    let object = Object::Prefix;
    let most = most.min(PREFIX_MOST);
    let got = bytes.len() as u64;
    if got < HEAD_LEN as u64 {
        return Err(Refused::Malformed { object, what: "shorter than a head", at: got });
    }
    let head = Head::decode(&bytes[..HEAD_LEN], namespace, check)?;
    // `manifest_len` is at most MANIFEST_MOST (`Head::decode`): no overflow.
    let manifest_end = HEAD_LEN as u64 + head.manifest_len;
    if manifest_end > most {
        return Err(Refused::TooLarge { object, what: "its head and manifest", len: manifest_end, most });
    }
    if got < manifest_end {
        return Err(Refused::Malformed { object, what: "cut inside its manifest", at: got });
    }
    let mut rows = Vec::new_in(alloc);
    decode_manifest(&bytes[HEAD_LEN..manifest_end as usize], &head, &mut rows)?;
    // At most ROWS_MOST rows of at most SUMMARY_MOST each (`Row::decode`): no overflow.
    let total = rows.iter().fold(manifest_end, |sum, row| sum + row.summary_len);
    if total > most {
        return Err(Refused::TooLarge { object, what: "its length, as its head and rows say", len: total, most });
    }
    if got != total {
        return Err(Refused::Malformed { object, what: "not the length its head and rows say", at: got });
    }
    let mut summaries = Vec::with_capacity_in(rows.len(), alloc);
    let mut at = manifest_end as usize;
    for row in rows.iter() {
        let end = at + row.summary_len as usize;
        summaries.push(Summary::decode(&bytes[at..end], row, alloc)?);
        at = end;
    }
    Ok(Prefixed { head, rows, summaries })
}
