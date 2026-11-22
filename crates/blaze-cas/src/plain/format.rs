//! The plain store's bytes on disk (the layout is in the module's docs, [`super`]): its head's
//! two slots, a generation's manifest, a segment's header, index entry and footer, the one
//! writer of a segment's layout ([`seal_parts`], behind [`seal_segment`] and the commit), its
//! one check ([`check_segment`]), and the pure work over sorted runs of entries (find, merge).
//! Nothing here does IO.
//!
//! The vocabulary is blaze-keyed's (rule 30): [`Namespace`] and [`Tag`] from `kind`, the
//! segment constants ([`HEADER`], [`FOOTER`], [`ENTRY`], [`ALIGN`]), [`SegmentId`] and
//! [`StoreError`] from `store`, [`RecordRef`] from `exec`.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_format::rows;
use blaze_ids::{ContentId, crc32c};
use blaze_keyed::{
    exec::RecordRef,
    kind::{Namespace, Tag},
    store::{ALIGN, ENTRY, FOOTER, HEADER, Holder, RowRefused, SEGMENT_MOST, SegmentId, StoreError, TwoValues},
};

/// The format of a plain store's own files: its head slots and its manifests. Formats 1 and 2
/// shared their number with the segments'; format 3 gives each retired row a mark, "lost, not
/// yet reported" ([`LostMark`]), in the u32 that format 2 kept zero; format 4 adds a segment
/// row's flag 4, sealed ([`Segment::sealed`]), which format 3 refused as a row no commit
/// writes; format 5 gives each root an expiry ([`Root::expires_ns`]) and adds the leases a
/// running command holds ([`Lease`], counted in the header's last word, which format 4 kept
/// zero), and a segment row's touch ([`Segment::touched`], a u64 at its end). A head or a manifest of another format is refused naming both; there is no
/// migration: the store is rebuilt from what made it.
pub const FORMAT: u32 = 5;

/// The format of a plain segment, which outlives any one store (a published segment is shared
/// through a bucket): its own number, so a change to the store's own files never changes a
/// segment's bytes or digest. Format 1 put a kind's byte in an entry's place (bits 56..64);
/// format 2 puts blaze-keyed's [`Tag`] there (bits 40..56, the u16 little-endian at bytes
/// 37..39 of the entry); format 3 has the bytes of format 2 and a new meaning of a reserved
/// tag (blaze-keyed `ReservedTag`): an expansion record has its own (`u16::MAX - 9`), where
/// format 2 filed it under the index's (`u16::MAX - 6`), which the reach walk follows. A
/// segment of another format is refused naming both; a home's store of format 2 is another
/// form's folder, reported and rebuilt (blaze-cli store/forms.rs), never read.
pub const SEGMENT_FORMAT: u32 = 3;

/// EVERY FORM A BLAZE MUST AGREE ON TO READ A STORE'S FOLDER (D160), each with the letter its
/// number takes in the folder's name: `f` the store's own files ([`FORMAT`]), `s` its segments
/// ([`SEGMENT_FORMAT`]). A store's folder is named by all of them ([`PlaceName`]): a blaze of
/// another form opens (makes) a folder of its own beside it, and never stops because another
/// blaze wrote a store. A format constant of the store's files added to this file and not here
/// fails the test `every_store_format_names_the_place` (plain/tests.rs), which counts them.
///
/// A record's own form inside a segment (a startup image's, plain/image.rs; a drive record's)
/// is not here: a record is found by its key, and its form is its key's (rule 21).
pub const FORMS: [(u8, u32); 2] = [(b'f', FORMAT), (b's', SEGMENT_FORMAT)];

/// The name of a store's folder of one set of [`FORMS`]: each form's letter and its number in
/// decimal, in order: `f5s3` for this blaze. Why letters and numbers, not a digest of them: the
/// name says to a person (and to the note that lists other folders) which forms a folder holds,
/// a newer one sorts after an older one of the same width, and the forms are few (a digest would
/// hide them for nothing: the name is no key, and nothing is shared by it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlaceName {
    bytes: [u8; PlaceName::MOST],
    len: u8,
}

impl PlaceName {
    /// The longest name: each form a letter and at most ten digits.
    const MOST: usize = FORMS.len() * 11;

    /// This blaze's folder: [`FORMS`] as they are.
    pub const THIS: PlaceName = PlaceName::of(FORMS);

    /// The name of the folder of `forms`.
    pub const fn of(forms: [(u8, u32); FORMS.len()]) -> PlaceName {
        let mut bytes = [0u8; PlaceName::MOST];
        let mut len = 0;
        let mut i = 0;
        while i < forms.len() {
            bytes[len] = forms[i].0;
            len += 1;
            let n = forms[i].1;
            let mut digits = 1;
            let mut rest = n / 10;
            while rest > 0 {
                digits += 1;
                rest /= 10;
            }
            let mut d = 0;
            let mut rest = n;
            while d < digits {
                bytes[len + digits - 1 - d] = b'0' + (rest % 10) as u8;
                rest /= 10;
                d += 1;
            }
            len += digits;
            i += 1;
        }
        PlaceName { bytes, len: len as u8 }
    }

    /// The name, in a const too (blaze-cli joins it to the store's root at compile time).
    pub const fn as_str(&self) -> &str {
        match core::str::from_utf8(self.bytes.split_at(self.len as usize).0) {
            Ok(it) => it,
            Err(_) => panic!("a place name is letters and digits"),
        }
    }

    /// The forms a folder named `leaf` holds, when it is a store's folder name: each of
    /// [`FORMS`]' letters in order, each followed by a decimal number with no leading zero.
    /// Anything else is no store's folder (`None`), and nothing reads, notes or removes it.
    pub fn parse(leaf: &str) -> Option<[(u8, u32); FORMS.len()]> {
        let mut out = FORMS;
        let mut rest = leaf.as_bytes();
        for slot in out.iter_mut() {
            let (&letter, after) = rest.split_first()?;
            if letter != slot.0 {
                return None;
            }
            let digits = after.iter().take_while(|it| it.is_ascii_digit()).count();
            if digits == 0 || digits > 10 || (digits > 1 && after[0] == b'0') {
                return None;
            }
            let number: u64 = after[..digits].iter().fold(0u64, |n, d| n * 10 + u64::from(d - b'0'));
            slot.1 = u32::try_from(number).ok()?;
            rest = &after[digits..];
        }
        rest.is_empty().then_some(out)
    }
}
/// What a head slot starts with.
pub(crate) const HEAD_MAGIC: [u8; 4] = *b"BLPH";

/// What a manifest starts with.
pub(crate) const MANIFEST_MAGIC: [u8; 4] = *b"BLPM";

/// One head slot's bytes.
pub(crate) const SLOT: usize = 64;

/// Where each of the two slots lies in the head file: a page apart, so the write of one never
/// shares a sector or a page with the other.
pub(crate) const SLOT_AT: [u64; 2] = [0, 4096];

/// The head file's length.
pub(crate) const HEAD_LEN: usize = 4096 + SLOT;

/// The longest offset a place holds: 2^40 units of [`ALIGN`] (16 TiB a segment).
pub(crate) const OFFSET_MOST: u64 = (1 << 40) * ALIGN;

/// The manifest's fixed part.
const MANIFEST_HEADER: usize = 32;

/// One segment's row in a manifest.
const SEGMENT_ROW: usize = 88;

/// `at` rounded up to [`ALIGN`].
pub const fn aligned(at: u64) -> u64 {
    at.div_ceil(ALIGN) * ALIGN
}

/// The CRC-32C an entry holds: of its key, its tag (u16, little-endian) and its payload, so an
/// entry that points at another record's bytes, or names another kind, fails it. Computed at a
/// put and compared only by a scrub (decision 18: a read checks nothing).
pub fn record_crc(key: &ContentId, tag: Tag, payload: &[u8]) -> u32 {
    let crc = crc32c::extend(crc32c::crc32c(&key.0), &tag.0.to_le_bytes());
    crc32c::extend(crc, payload)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    ContentId(bytes[at..at + 32].try_into().expect("32 bytes"))
}

/// The first eight bytes of a hash of `parts`, little-endian: a segment's or a log's id.
pub(crate) fn id_of(domain: &'static str, parts: &[&[u8]]) -> u64 {
    let mut hasher = blaze_ids::Hasher::new(domain);
    for part in parts {
        hasher.update(part);
    }
    let id: ContentId = hasher.finish();
    u64::from_le_bytes(id.0[0..8].try_into().expect("8 bytes"))
}

/// One committed generation, as a head slot names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Slot {
    pub generation: u64,
    /// The manifest log's id (`manifests-<id>`), which holds the manifest.
    pub log: u64,
    pub manifest_at: u64,
    pub manifest_len: u32,
    pub manifest_crc: u32,
}

/// What one slot's bytes are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SlotRead {
    /// Never written: all zeros.
    Blank,
    /// Of this layout, its checksum failing: a flip the power cut short.
    Torn,
    /// Not this layout's: another store's file.
    Foreign,
    /// This layout's, whole, of another format.
    Format { found: u32 },
    Slot(Slot),
}

impl Slot {
    /// Bytes 0..4 magic `BLPH`, 4..8 format, 8..16 generation, 16..24 manifest log id, 24..32
    /// manifest offset, 32..36 manifest length, 36..40 manifest CRC-32C, 40..60 zero, 60..64
    /// CRC-32C of bytes 0..60. Little-endian.
    pub(crate) fn encode(&self) -> [u8; SLOT] {
        self.encode_as(FORMAT)
    }

    /// The slot as a store of `format` wrote it: for the test of the previous layout.
    pub(crate) fn encode_as(&self, format: u32) -> [u8; SLOT] {
        let mut out = [0u8; SLOT];
        out[0..4].copy_from_slice(&HEAD_MAGIC);
        out[4..8].copy_from_slice(&format.to_le_bytes());
        out[8..16].copy_from_slice(&self.generation.to_le_bytes());
        out[16..24].copy_from_slice(&self.log.to_le_bytes());
        out[24..32].copy_from_slice(&self.manifest_at.to_le_bytes());
        out[32..36].copy_from_slice(&self.manifest_len.to_le_bytes());
        out[36..40].copy_from_slice(&self.manifest_crc.to_le_bytes());
        let crc = crc32c::crc32c(&out[..60]);
        out[60..64].copy_from_slice(&crc.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> SlotRead {
        let bytes = &bytes[..SLOT];
        if bytes.iter().all(|it| *it == 0) {
            return SlotRead::Blank;
        }
        let magic: [u8; 4] = bytes[0..4].try_into().expect("4 bytes");
        // A slot the store wrote begins with its magic; a part of it (or none) and then zeros
        // is a write that landed in part, or a short head extended with zeros: torn, never
        // another store's.
        let same = magic.iter().zip(HEAD_MAGIC.iter()).take_while(|(a, b)| a == b).count();
        if same < 4 && magic[same..].iter().all(|it| *it == 0) {
            return SlotRead::Torn;
        }
        if magic != HEAD_MAGIC {
            return SlotRead::Foreign;
        }
        if crc32c::crc32c(&bytes[..60]) != u32_at(bytes, 60) {
            return SlotRead::Torn;
        }
        let found = u32_at(bytes, 4);
        if found != FORMAT {
            return SlotRead::Format { found };
        }
        SlotRead::Slot(Slot {
            generation: u64_at(bytes, 8),
            log: u64_at(bytes, 16),
            manifest_at: u64_at(bytes, 24),
            manifest_len: u32_at(bytes, 32),
            manifest_crc: u32_at(bytes, 36),
        })
    }

    /// Where the next manifest of its log goes.
    pub(crate) fn log_end(&self) -> u64 {
        aligned(self.manifest_at + u64::from(self.manifest_len))
    }
}

/// What a head file says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum HeadRead {
    /// No whole slot: nothing committed here survives (never committed, or the first flip's
    /// slot torn by a power cut). What the directory holds is read from the files' own
    /// footers and reported.
    Fresh {
        /// The slot that was torn, if one was: a trace of a flip a power cut cut short.
        torn: Option<usize>,
    },
    /// The newest whole slot, and the other one when it is whole too: the generation to fall
    /// back to when the newest's manifest did not survive a power cut. `torn`: the other slot
    /// was torn, so a newer flip was cut short and what it named is read from the footers.
    Committed(Slot, Option<Slot>, Option<usize>),
    /// A whole slot of another format: a store of the previous layout (rule 26), refused by
    /// the format it is and the one this blaze reads, never read as empty.
    Version { slot: usize, found: u32 },
    /// Not a head this blaze reads: [`StoreError::Head`], what is wrong and at which byte.
    Refused(StoreError),
}

/// A head longer than any plain head ([`HEAD_LEN`]): another store's file (the
/// content-addressed store keeps a `head` of its own); the plain store needs a directory of its
/// own. `at` is the file's length.
pub(crate) const HEAD_TOO_LONG: &str = "longer than any plain store's head: another store's file; give the plain store a directory of its own";
/// Where a slot begins, another magic than a plain slot's: another store's file. `at` is the
/// slot's offset.
pub(crate) const HEAD_FOREIGN: &str = "another magic than a plain slot's where a slot begins: another store's file; give the plain store a directory of its own";
/// A whole slot holding a generation of the other slot's parity. `at` is the slot's offset.
pub(crate) const HEAD_PARITY: &str = "a slot holding a generation that belongs in the other slot";
/// Shorter than this process made it and synced it: cut outside blaze. `at` is its length.
pub(crate) const HEAD_CUT: &str = "shorter than this process made it and synced it: cut outside blaze";
/// What [`StoreError::Format`] says of a head slot of another format.
pub(crate) const HEAD_SLOT: &str = "a plain store's head slot";

/// What `bytes[at..at + SLOT]` holds when the head is `bytes`, which may end before it: a slot
/// only partly there is torn, unless what is there cannot begin a plain slot.
fn slot_in(bytes: &[u8], at: usize) -> SlotRead {
    if bytes.len() >= at + SLOT {
        return Slot::decode(&bytes[at..at + SLOT]);
    }
    if bytes.is_empty() {
        // No head at all: nothing was ever made here, no trace of a flip.
        return SlotRead::Blank;
    }
    // A head the store made is never short (made at its full length and synced before the
    // first flip); one cut short is a trace that what it held may be gone.
    let present = &bytes[at.min(bytes.len())..];
    let n = present.len().min(4);
    if present[..n] != HEAD_MAGIC[..n] && present[..n].iter().any(|it| *it != 0) {
        return SlotRead::Foreign;
    }
    SlotRead::Torn
}

/// The newest whole slot of `bytes`, the head file as read (empty when it is not there). A
/// head shorter than [`HEAD_LEN`] is read as far as it goes: its missing slots are torn. It is
/// another store's file only when its bytes say so (another magic where a slot begins), never
/// for its length alone, except a file longer than any plain head.
pub(crate) fn read_head(bytes: &[u8]) -> HeadRead {
    if bytes.len() > HEAD_LEN {
        return HeadRead::Refused(StoreError::Head { what: HEAD_TOO_LONG, at: bytes.len() as u64 });
    }
    let slots = [slot_in(bytes, SLOT_AT[0] as usize), slot_in(bytes, SLOT_AT[1] as usize)];
    let mut whole: [Option<Slot>; 2] = [None, None];
    let mut torn = None;
    for (at, read) in slots.iter().enumerate() {
        match *read {
            SlotRead::Blank => {}
            SlotRead::Torn => torn = Some(at),
            SlotRead::Foreign => {
                return HeadRead::Refused(StoreError::Head { what: HEAD_FOREIGN, at: SLOT_AT[at] });
            }
            SlotRead::Format { found } => return HeadRead::Version { slot: at, found },
            SlotRead::Slot(slot) => {
                if slot.generation % 2 != at as u64 {
                    return HeadRead::Refused(StoreError::Head { what: HEAD_PARITY, at: SLOT_AT[at] });
                }
                whole[at] = Some(slot);
            }
        }
    }
    let [a, b] = whole;
    let order = match (a, b) {
        (Some(a), Some(b)) if b.generation > a.generation => Some((b, Some(a))),
        (Some(a), b) => Some((a, b)),
        (None, Some(b)) => Some((b, None)),
        (None, None) => None,
    };
    match order {
        Some((newest, older)) => HeadRead::Committed(newest, older, torn),
        // The head is made at its full length, zeros, and synced at the store's first open,
        // before anything is written; a flip writes only the slot the newest does not hold,
        // in place, inside one sector. So a tear destroys at most the slot being written:
        // none whole means the first flip's slot (or the first two's) did not survive.
        None => HeadRead::Fresh { torn },
    }
}

/// What a segment's header starts with.
pub(crate) const SEGMENT_MAGIC: [u8; 4] = *b"BLPS";

/// What a segment's footer starts with.
pub(crate) const FOOTER_MAGIC: [u8; 4] = *b"BLPE";

/// The BLAKE3 domain of a segment's digest: its name in a bucket, what a signed statement binds.
pub(crate) const SEGMENT_DIGEST: &str = "blaze plain store segment 1";

/// What a segment's header and footer say, read and checked ([`check_segment`]), or read by
/// range and not yet checked ([`describe`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Described {
    pub namespace: Namespace,
    pub count: u32,
    pub index_at: u64,
    pub digest: ContentId,
}

/// The digest a segment's footer holds: of `bytes[..len - 32]`.
pub(crate) fn segment_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(SEGMENT_DIGEST, &bytes[..bytes.len() - 32])
}

/// A segment's header for `run` (sorted entries) at `index_at` in `namespace`: 0..4 `BLPS`,
/// 4..8 format, 8..16 namespace, 16..20 entries, 20..24 zero, 24..32 the index's offset, 32..64
/// the lowest key, 64..96 the highest, 96..128 zero. The payloads follow at [`HEADER`].
pub(crate) fn header(namespace: Namespace, run: &[u8], index_at: u64) -> [u8; HEADER] {
    let count = run.len() / ENTRY;
    // An empty run's keys are zero: the bytes of an all-zero id.
    let (lowest, highest) = if count == 0 { (ContentId([0; 32]), ContentId([0; 32])) } else { (id_at(run, 0), id_at(run, (count - 1) * ENTRY)) };
    header_of(namespace, count as u32, lowest, highest, index_at)
}

/// THE ONE HOME of a segment's header bytes ([`header`]'s layout): `count` entries, the
/// lowest key `lowest` and the highest `highest` (zero for an empty run), the index at
/// `index_at`. A streamed copy says these before its run exists (it counted them in its first
/// walk).
pub(crate) fn header_of(namespace: Namespace, count: u32, lowest: ContentId, highest: ContentId, index_at: u64) -> [u8; HEADER] {
    let mut out = [0u8; HEADER];
    out[0..4].copy_from_slice(&SEGMENT_MAGIC);
    out[4..8].copy_from_slice(&SEGMENT_FORMAT.to_le_bytes());
    out[8..16].copy_from_slice(&namespace.0.to_le_bytes());
    out[16..20].copy_from_slice(&count.to_le_bytes());
    out[24..32].copy_from_slice(&index_at.to_le_bytes());
    out[32..64].copy_from_slice(&lowest.0);
    out[64..96].copy_from_slice(&highest.0);
    out
}

/// The footer's first 16 bytes (0..4 `BLPE`, 4..8 entries, 8..16 the index's offset); the
/// digest of every byte before it follows at 16..48.
pub(crate) fn footer_head(count: u32, index_at: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&FOOTER_MAGIC);
    out[4..8].copy_from_slice(&count.to_le_bytes());
    out[8..16].copy_from_slice(&index_at.to_le_bytes());
    out
}

/// The one writer of a segment's layout: its header, footer and digest, for `run` (sorted
/// entries, each placing its payload in `payloads`, which lie end to end from [`HEADER`] to
/// `index_at`) in `namespace`. The commit streams the payloads it holds into their writes
/// without a copy; [`seal_segment`] lays them out in one buffer; both through this.
pub(crate) struct Sealed {
    pub header: [u8; HEADER],
    /// The footer, its digest included.
    pub footer: [u8; FOOTER],
    pub described: Described,
    /// The file's length.
    pub len: u64,
}

pub(crate) fn seal_parts<'p>(namespace: Namespace, run: &[u8], index_at: u64, payloads: impl IntoIterator<Item = &'p [u8]>) -> Sealed {
    let count = (run.len() / ENTRY) as u32;
    let header = header(namespace, run, index_at);
    let foot = footer_head(count, index_at);
    let mut hasher = blaze_ids::Hasher::new(SEGMENT_DIGEST);
    hasher.update(&header);
    let mut at = HEADER as u64;
    for bytes in payloads {
        hasher.update(bytes);
        at += bytes.len() as u64;
    }
    debug_assert_eq!(at, index_at, "the index follows the payloads");
    hasher.update(run);
    hasher.update(&foot);
    let digest: ContentId = hasher.finish();
    let mut footer = [0u8; FOOTER];
    footer[..16].copy_from_slice(&foot);
    footer[16..].copy_from_slice(&digest.0);
    let len = index_at + run.len() as u64 + FOOTER as u64;
    Sealed { header, footer, described: Described { namespace, count, index_at, digest }, len }
}

/// A whole segment of `namespace` holding `records` (each its key and tag, and its payload):
/// the payloads and the index in key order, the first given of a duplicate key kept (the rest
/// dropped unread: one key names one value, rule 21, which the store's put and a push's
/// comparison enforce), laid out into `out` (cleared first) by the store's one writer
/// ([`seal_parts`]). What it wrote, described. A commit of the same records, put in any order
/// on any workers, writes the same bytes.
///
/// A segment of zero records is valid: header, no payloads, an empty index at [`HEADER`], the
/// footer (176 bytes); [`check_segment`] accepts it and reads no record from it.
pub fn seal_segment<A: core::alloc::Allocator>(namespace: Namespace, records: &[(RecordRef, &[u8])], out: &mut Vec<u8, A>) -> Result<Described, StoreError> {
    let mut order: Vec<usize, Global> = Vec::with_capacity_in(records.len(), Global);
    order.extend(0..records.len());
    // Stable: of two records of one key the first given stays first, and is the one kept.
    order.sort_by(|a, b| records[*a].0.key.cmp(&records[*b].0.key));
    order.dedup_by(|b, a| records[*a].0.key == records[*b].0.key);
    // The payloads lie in key order, as a commit lays them: the bytes are the records', not
    // the order they came in (rule 25).
    let laid = order;
    let mut entries: Vec<Entry, Global> = Vec::with_capacity_in(laid.len(), Global);
    let mut at = HEADER as u64;
    for &i in &laid {
        let (record, payload) = records[i];
        // A record of 4 GiB or more, or records past what an entry's offset holds (16 TiB).
        let too_long = StoreError::TooLong { tag: record.tag, key: record.key, len: payload.len() as u64 };
        let len = u32::try_from(payload.len()).map_err(|_| too_long.clone())?;
        if at + u64::from(len) > OFFSET_MOST {
            return Err(too_long);
        }
        entries.push(Entry { key: record.key, tag: record.tag, at, len, crc: record_crc(&record.key, record.tag, payload) });
        at = aligned(at + u64::from(len));
    }
    let index_at = at;
    entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    let mut run: Vec<u8, Global> = Vec::with_capacity_in(entries.len() * ENTRY, Global);
    for entry in &entries {
        entry.encode_into(&mut run);
    }
    let order = laid;
    let sealed = seal_parts(namespace, &run, index_at, Padded::new(records, &order));
    out.clear();
    out.reserve(sealed.len as usize);
    out.extend_from_slice(&sealed.header);
    for &i in &order {
        let payload = records[i].1;
        out.extend_from_slice(payload);
        out.resize(aligned(out.len() as u64) as usize, 0);
    }
    debug_assert_eq!(out.len() as u64, index_at);
    out.extend_from_slice(&run);
    out.extend_from_slice(&sealed.footer);
    Ok(sealed.described)
}

/// The payloads of `order`, each followed by the zeros to the next [`ALIGN`]: what lies from
/// [`HEADER`] to the index, as slices for the digest.
struct Padded<'r, 'p> {
    records: &'r [(RecordRef, &'p [u8])],
    order: &'r [usize],
    next: usize,
    pad: bool,
}

impl<'r, 'p> Padded<'r, 'p> {
    fn new(records: &'r [(RecordRef, &'p [u8])], order: &'r [usize]) -> Padded<'r, 'p> {
        Padded { records, order, next: 0, pad: false }
    }
}

const ZEROS: [u8; ALIGN as usize] = [0; ALIGN as usize];

impl<'r, 'p> Iterator for Padded<'r, 'p> {
    type Item = &'p [u8];

    fn next(&mut self) -> Option<&'p [u8]> {
        loop {
            let &i = self.order.get(self.next)?;
            let payload = self.records[i].1;
            if !self.pad {
                self.pad = true;
                return Some(payload);
            }
            self.pad = false;
            self.next += 1;
            let rest = (aligned(payload.len() as u64) - payload.len() as u64) as usize;
            if rest > 0 {
                return Some(&ZEROS[..rest]);
            }
        }
    }
}

/// The entries a footer says its segment holds, or `None` when the bytes are not a footer.
pub(crate) fn footer_count(bytes: &[u8]) -> Option<u32> {
    (bytes.len() == FOOTER && bytes[0..4] == FOOTER_MAGIC).then(|| u32_at(bytes, 4))
}

/// Checks a whole segment's bytes before anything maps or reads them: its header and footer
/// agree, its length is what they say, its digest is the footer's, and every entry is sorted
/// (each key once) and inside the payloads. A fetched segment passes this before it is
/// written; a segment of an earlier boot before it is read. The segment it names is the
/// footer's digest (what the bytes claim to be); a segment of another format is
/// [`StoreError::Format`], naming the format it is and the one read (rule 26: never read as
/// empty).
pub fn check_segment(bytes: &[u8]) -> Result<Described, StoreError> {
    let len = bytes.len();
    let claimed = SegmentId(if len >= FOOTER { id_at(bytes, len - 32) } else { ContentId([0; 32]) });
    let layout = |what: &'static str, at: u64| StoreError::Layout { segment: claimed, what, at };
    if len < HEADER + FOOTER || len as u64 > SEGMENT_MOST {
        return Err(layout("a segment's length is outside HEADER + FOOTER ..= 4 GiB", len as u64));
    }
    if bytes[0..4] != SEGMENT_MAGIC {
        return Err(layout("its header does not start with `BLPS`: not a plain segment", 0));
    }
    if u32_at(bytes, 4) != SEGMENT_FORMAT {
        return Err(StoreError::Format { what: "a plain store's segment", found: u32_at(bytes, 4), wanted: SEGMENT_FORMAT });
    }
    let foot = &bytes[len - FOOTER..];
    let (count, index_at) = (u32_at(bytes, 16), u64_at(bytes, 24));
    if foot[0..4] != FOOTER_MAGIC || u32_at(foot, 4) != count || u64_at(foot, 8) != index_at {
        return Err(layout("its footer does not say what its header says: it is cut short or torn", (len - FOOTER) as u64));
    }
    let end = index_at.checked_add(u64::from(count) * ENTRY as u64).and_then(|it| it.checked_add(FOOTER as u64));
    if index_at < HEADER as u64 || !index_at.is_multiple_of(ALIGN) || end != Some(len as u64) {
        return Err(layout("its index does not start aligned after the header and end at the footer", index_at));
    }
    if segment_digest(bytes) != claimed.0 {
        return Err(layout("its bytes are not the ones its digest names: torn, cut short or forged", 0));
    }
    let run = &bytes[index_at as usize..index_at as usize + count as usize * ENTRY];
    for i in 0..count as usize {
        let at = index_at + (i * ENTRY) as u64;
        let entry = Entry::decode(&run[i * ENTRY..(i + 1) * ENTRY]).map_err(|what| layout(what, at))?;
        if entry.at < HEADER as u64 || !entry.at.is_multiple_of(ALIGN) || entry.at + u64::from(entry.len) > index_at {
            return Err(layout("an entry places its payload outside the payloads", at));
        }
        if i > 0 && run[(i - 1) * ENTRY..(i - 1) * ENTRY + 32] >= run[i * ENTRY..i * ENTRY + 32] {
            return Err(layout("an entry is not after the one before it: the index is not sorted, each key once", at));
        }
    }
    if count > 0 && (bytes[32..64] != run[0..32] || bytes[64..96] != run[(count as usize - 1) * ENTRY..(count as usize - 1) * ENTRY + 32]) {
        return Err(layout("its header's lowest or highest key is not its index's", 32));
    }
    Ok(Described { namespace: Namespace(u64_at(bytes, 8)), count, index_at, digest: claimed.0 })
}

/// What a segment's header and footer say, read by range without its middle (a remote
/// segment's description before anything else of it is fetched); `None` when they are not a
/// plain segment's of this format or disagree. Untrusted until [`check_segment`] passes on the
/// whole.
pub fn describe(head: &[u8], foot: &[u8], len: u64) -> Option<Described> {
    if head.len() != HEADER || foot.len() != FOOTER || head[0..4] != SEGMENT_MAGIC || u32_at(head, 4) != SEGMENT_FORMAT || foot[0..4] != FOOTER_MAGIC {
        return None;
    }
    let (count, index_at) = (u32_at(head, 16), u64_at(head, 24));
    let end = index_at.checked_add(u64::from(count) * ENTRY as u64)?.checked_add(FOOTER as u64)?;
    (u32_at(foot, 4) == count && u64_at(foot, 8) == index_at && end == len).then(|| Described {
        namespace: Namespace(u64_at(head, 8)),
        count,
        index_at,
        digest: id_at(foot, 16),
    })
}

/// A segment as a manifest names it: what it is (its digest, length, index, namespace), and
/// what this machine knows of it (the generation that added it, the boot it was checked in,
/// whether it is here, whether it is published).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment {
    /// The digest's first eight bytes: its file's name here (`seg-<id:016x>`).
    pub id: u64,
    pub digest: ContentId,
    pub len: u64,
    pub index_at: u64,
    pub count: u32,
    pub namespace: Namespace,
    /// The generation that added it: compaction prunes only what is older than a window.
    pub generation: u64,
    /// The machine boot it was written in or last checked whole in ([`super::core`]): a
    /// segment of another boot is checked once before it is read, as a power cut may have lost
    /// what the kernel had not written back.
    pub boot: u64,
    /// Its file is here; else it is named for a source to fetch on a read.
    pub local: bool,
    /// It is in a shared bucket (pulled, or pushed): compaction never rewrites it, it only
    /// lets go of this machine's copy.
    pub published: bool,
    /// Its bytes are on the media, in order before the flip that set this (store-next.md, the
    /// exit tail): a barrier covered its file and completed before that flip was asked, or an
    /// open of a later boot read it whole from the disk. An open of a new boot proves it by
    /// this row with no byte read ([`super::verified::Verified::sealed`]); one not sealed, the
    /// tail of the boot before, is checked whole as before.
    pub sealed: bool,
    /// The newest generation whose commit set a root naming a record of it that the commit
    /// did not write (a record reused by key): a cleanup slice planned before that generation
    /// may have judged the record dead, so it does not replace this segment ([`super::core`],
    /// the reach sweep). Zero: never.
    pub touched: u64,
}

impl Segment {
    /// The segment `described` says, of `len` bytes, as a manifest row holds it before it is
    /// named (generation, boot: zero; here, not published).
    pub fn of(described: &Described, len: u64) -> Segment {
        Segment {
            id: u64::from_le_bytes(described.digest.0[0..8].try_into().expect("8 bytes")),
            digest: described.digest,
            len,
            index_at: described.index_at,
            count: described.count,
            namespace: described.namespace,
            generation: 0,
            boot: 0,
            local: true,
            published: false,
            sealed: false,
            touched: 0,
        }
    }

    pub(crate) fn file_len(&self) -> u64 {
        self.len
    }

    /// Its sorted run of entries, out of the file's bytes (`file`: the whole segment).
    pub fn run<'a>(&self, file: &'a [u8]) -> &'a [u8] {
        &file[self.index_at as usize..self.index_at as usize + self.count as usize * ENTRY]
    }
}

/// THE SHAPE A SEGMENT MAY HAVE, the one home of it (rules 74, 76): a file of `len` bytes
/// whose index of `count` entries starts at `index_at` is at most `most` bytes (the store's
/// segment cap, [`SEGMENT_MOST`] outside a simulation), its index starts at a multiple of
/// [`ALIGN`], and the index and the footer end at its length. What a commit's layout and a
/// merge's copy ask before a byte of the segment is written, and what [`Row::new`] asks of a
/// row before it is written into a manifest or read out of one.
pub(crate) fn shape(len: u64, index_at: u64, count: u32, most: u64) -> Result<(), RowRefused> {
    if len > most {
        return Err(RowRefused::TooLong { len, most });
    }
    if !index_at.is_multiple_of(ALIGN) {
        return Err(RowRefused::Unaligned { index_at, align: ALIGN });
    }
    let end = index_at.checked_add(u64::from(count) * ENTRY as u64 + FOOTER as u64);
    if end != Some(len) {
        return Err(RowRefused::NotItsEnd { index_at, count, end, len });
    }
    Ok(())
}

/// A segment row's fields as a manifest's bytes hold them: its flags as their word (1 here,
/// 2 published, 4 sealed), so a word no commit writes can be named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RowFields {
    pub digest: ContentId,
    pub len: u64,
    pub index_at: u64,
    pub count: u32,
    pub flags: u32,
    pub namespace: Namespace,
    pub generation: u64,
    pub boot: u64,
    pub touched: u64,
}

impl RowFields {
    /// `seg`'s fields, its flags as their word.
    pub(crate) fn of(seg: &Segment) -> RowFields {
        RowFields {
            digest: seg.digest,
            len: seg.len,
            index_at: seg.index_at,
            count: seg.count,
            flags: u32::from(seg.local) | u32::from(seg.published) << 1 | u32::from(seg.sealed) << 2,
            namespace: seg.namespace,
            generation: seg.generation,
            boot: seg.boot,
            touched: seg.touched,
        }
    }

    /// The fields of the row at `bytes[at..at + SEGMENT_ROW]`.
    fn read(bytes: &[u8], at: usize) -> RowFields {
        RowFields {
            digest: id_at(bytes, at),
            len: u64_at(bytes, at + 32),
            index_at: u64_at(bytes, at + 40),
            count: u32_at(bytes, at + 48),
            flags: u32_at(bytes, at + 52),
            namespace: Namespace(u64_at(bytes, at + 56)),
            generation: u64_at(bytes, at + 64),
            boot: u64_at(bytes, at + 72),
            touched: u64_at(bytes, at + 80),
        }
    }
}

/// A ROW A COMMIT WRITES: the one definition of it (rules 74, 76). Its only constructor,
/// [`Row::new`], checks it, and a manifest's bytes hold only rows of this type: the commit
/// encodes through it ([`Manifest::encode`]) and the open decodes through it
/// ([`Manifest::decode`]), so a manifest its own open would refuse is never written; the flip
/// that would write one is refused, naming the segment and what is wrong with its row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Row(RowFields);

impl Row {
    /// `fields` as a row of a manifest of generation `manifest`, in a store whose segments are
    /// at most `most` bytes: its [`shape`], a generation and a touch no newer than the
    /// manifest's, flags at most 7. Else why not, with the values.
    pub(crate) fn new(fields: RowFields, manifest: u64, most: u64) -> Result<Row, RowRefused> {
        shape(fields.len, fields.index_at, fields.count, most)?;
        if fields.generation > manifest {
            return Err(RowRefused::NewerGeneration { row: fields.generation, manifest });
        }
        if fields.touched > manifest {
            return Err(RowRefused::NewerTouch { row: fields.touched, manifest });
        }
        if fields.flags > 7 {
            return Err(RowRefused::Flags { flags: fields.flags });
        }
        Ok(Row(fields))
    }

    /// The segment the row names.
    pub(crate) fn segment(&self) -> Segment {
        let it = self.0;
        Segment {
            id: u64::from_le_bytes(it.digest.0[0..8].try_into().expect("8 bytes")),
            digest: it.digest,
            len: it.len,
            index_at: it.index_at,
            count: it.count,
            local: it.flags & 1 != 0,
            published: it.flags & 2 != 0,
            sealed: it.flags & 4 != 0,
            namespace: it.namespace,
            generation: it.generation,
            boot: it.boot,
            touched: it.touched,
        }
    }

    /// Its [`SEGMENT_ROW`] bytes, the one writer of a row.
    fn encode_into<A: core::alloc::Allocator>(&self, out: &mut Vec<u8, A>) {
        let it = self.0;
        out.extend_from_slice(&it.digest.0);
        out.extend_from_slice(&it.len.to_le_bytes());
        out.extend_from_slice(&it.index_at.to_le_bytes());
        out.extend_from_slice(&it.count.to_le_bytes());
        out.extend_from_slice(&it.flags.to_le_bytes());
        out.extend_from_slice(&it.namespace.0.to_le_bytes());
        out.extend_from_slice(&it.generation.to_le_bytes());
        out.extend_from_slice(&it.boot.to_le_bytes());
        out.extend_from_slice(&it.touched.to_le_bytes());
    }
}

/// The position of `key` in a sorted run of `count` entries that is read by range (a remote
/// shard, by ranged GET): `read(at, len)` returns those bytes of the run. Interpolation picks a
/// window of [`PROBE_WINDOW`] entries around where the key should be; the key is found or
/// shown absent inside it, else the window's ends narrow the search and the next read is a
/// bisection window. For a shard of a million uniform keys the first window holds the key's
/// place with high likelihood (the guess is off by about `sqrt(n)` = 1000 entries, the window
/// is 1365): one ranged read, two at the tail, at most `2 + log2(n / window)` = 12.
pub(crate) fn find_ranged(
    segment: SegmentId,
    count: u32,
    key: &ContentId,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), StoreError>,
) -> Result<Option<(u32, Entry)>, StoreError> {
    let want = u64::from_be_bytes(key.0[0..8].try_into().expect("8 bytes"));
    let (mut lo, mut hi) = (0u64, u64::from(count));
    let (mut lo_prefix, mut hi_prefix) = (0u64, u64::MAX);
    let mut reads = 0;
    let mut bytes: Vec<u8, Global> = Vec::with_capacity_in((PROBE_WINDOW as usize) * ENTRY, Global);
    while lo < hi {
        let span = hi - lo;
        let window = PROBE_WINDOW.min(span);
        let guess = if reads < 2 {
            let prefix_span = (hi_prefix - lo_prefix).max(1);
            // The remaining row count is bounded by the segment's u32 count.
            lo + rows::interpolated_offset(want.saturating_sub(lo_prefix), prefix_span, span as u32)
        } else {
            lo + span / 2
        };
        let start = guess.saturating_sub(window / 2).max(lo).min(hi - window);
        reads += 1;
        bytes.clear();
        bytes.resize((window * ENTRY as u64) as usize, 0);
        read(start * ENTRY as u64, &mut bytes)?;
        let first = &bytes[0..32];
        let last = &bytes[bytes.len() - ENTRY..bytes.len() - ENTRY + 32];
        if key.0[..] < *first {
            hi = start;
            hi_prefix = u64::from_be_bytes(first[0..8].try_into().expect("8 bytes"));
            continue;
        }
        if key.0[..] > *last {
            lo = start + window;
            lo_prefix = u64::from_be_bytes(last[0..8].try_into().expect("8 bytes"));
            continue;
        }
        return match find(&bytes, key) {
            Some(i) => {
                let entry = Entry::decode(&bytes[i * ENTRY..(i + 1) * ENTRY])
                    .map_err(|what| StoreError::Layout { segment, what, at: (start + i as u64) * ENTRY as u64 })?;
                Ok(Some((start as u32 + i as u32, entry)))
            }
            None => Ok(None),
        };
    }
    Ok(None)
}

/// Entries one ranged read of a remote index takes: 64 KiB.
pub(crate) const PROBE_WINDOW: u64 = (64 << 10) / ENTRY as u64;

/// A file no generation names any more, kept until no process can still be about to open it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Retired {
    pub id: u64,
    /// A manifest log, else a segment.
    pub log: bool,
    /// The wall clock when it was retired; it is unlinked a grace period later, and never while
    /// it is marked lost.
    pub since_ns: u64,
    /// A segment retired because an open found it lost, its loss not yet reported: set in the
    /// manifest that retires it, so the retirement and the report are one durable fact; every
    /// open that reads it reports it, whoever wrote it, and only a flip made after a report of
    /// it was handed to its caller clears it ([`super::core::Plain::handed`]).
    pub lost: Option<LostMark>,
}

/// Why an open found a segment lost, as a retired row keeps it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LostKind {
    /// Named by no whole generation: the one that did was lost in a power cut.
    Orphan = 1,
    /// Named by a generation of an earlier boot and not whole at its end: a power cut before
    /// the kernel wrote it back.
    NotWhole = 2,
    /// Named by a generation of an earlier boot and gone: its directory entry was not written
    /// back before a power cut.
    Gone = 3,
}

/// "Lost, not yet reported": why, and how many records it held when that could be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LostMark {
    pub kind: LostKind,
    pub records: Option<u32>,
}

impl LostMark {
    /// The most records a mark can name; a segment holds fewer (4 GiB over [`ENTRY`] bytes).
    const RECORDS_MOST: u32 = (1 << 30) - 2;

    /// Bits 0..2 the kind (0: no mark), bits 2..32 the records plus one (0: not known).
    pub(crate) fn encode(mark: Option<LostMark>) -> u32 {
        match mark {
            None => 0,
            Some(mark) => {
                let records = mark.records.filter(|it| *it <= LostMark::RECORDS_MOST).map_or(0, |it| it + 1);
                mark.kind as u32 | records << 2
            }
        }
    }

    pub(crate) fn decode(word: u32) -> Result<Option<LostMark>, &'static str> {
        let kind = match word & 3 {
            0 if word == 0 => return Ok(None),
            0 => return Err("a retired row's mark names records and no kind"),
            1 => LostKind::Orphan,
            2 => LostKind::NotWhole,
            _ => LostKind::Gone,
        };
        let records = (word >> 2).checked_sub(1);
        Ok(Some(LostMark { kind, records }))
    }

    /// What a report of it says, as the open that found it said it.
    pub(crate) fn why(&self) -> String {
        match self.kind {
            LostKind::Orphan => "no whole generation names it: the generation that did was lost in a power cut, and its records with it".to_owned(),
            LostKind::NotWhole => "it was written in an earlier boot and did not survive its end whole: a power cut before the kernel wrote it back; its records are dropped".to_owned(),
            LostKind::Gone => "a generation named it, written in an earlier boot, and it is gone: a power cut before the kernel wrote its directory entry back; its records are dropped".to_owned(),
        }
    }
}

/// A named root: the store's own record (tag [`blaze_keyed::kind::ReservedTag::Root`]) listing the keys compaction
/// keeps for it, until `expires_ns` on the wall clock the flips pass in ([`NEVER`]: kept until
/// it is unset). A root past its expiry keeps nothing, and the next flip drops it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Root {
    pub name: String,
    pub key: ContentId,
    pub generation: u64,
    pub expires_ns: u64,
}

/// No expiry: a root set with no life ([`super::core::Plain::root`]) or a lease never let go.
pub(crate) const NEVER: u64 = u64::MAX;

/// What a running command holds while its records are not yet named by a root: every segment
/// of generation `since` or newer is kept whole, and no file retired after `opened_ns` (less
/// the grace) is unlinked, until `expires_ns`. Set by the holder's commits, let go by its last;
/// a process that dies holding one is let go of by the clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Lease {
    /// The holder's [`super::core::PlainConfig::nonce`].
    pub holder: u64,
    /// The generation the holder opened on, plus one: the first its commits can name.
    pub since: u64,
    /// The wall clock at the holder's open.
    pub opened_ns: u64,
    pub expires_ns: u64,
}

/// What one generation holds: its segments (oldest first), its roots, its leases, its retired
/// files.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Manifest {
    pub generation: u64,
    pub segments: Vec<Segment>,
    pub roots: Vec<Root>,
    pub leases: Vec<Lease>,
    pub retired: Vec<Retired>,
}

/// One lease row: holder u64, since u64, opened ns u64, expires ns u64.
const LEASE_ROW: usize = 32;

impl Manifest {
    /// Bytes 0..4 magic `BLPM`, 4..8 format, 8..16 generation, 16..20 segments `s`, 20..24
    /// retired `r`, 24..28 roots `t`, 28..32 leases `l`; then `s` x 88 (digest 32, length u64,
    /// index offset u64, entries u32, flags u32 (1 here, 2 published, 4 sealed), namespace u64,
    /// generation u64, boot u64, touched u64), `r` x 24 (id u64, 1 for a log u32, the mark u32
    /// ([`LostMark::encode`]: zero for none, never set on a log), since ns u64), `l` x 32
    /// ([`Lease`]: holder, since, opened ns, expires ns, each u64), and `t` roots, each 32 key,
    /// 8 generation, 8 expiry ns, 2 name length, the name's UTF-8, zeros to a multiple of 8.
    /// Little-endian.
    ///
    /// Each segment row is written through [`Row::new`], the check [`Manifest::decode`] reads
    /// it through, in a store whose segments are at most `most` bytes: a row the open would
    /// refuse is [`StoreError::Row`] here, naming the segment, its row's byte and why, and no
    /// byte of the manifest is handed on.
    pub(crate) fn encode(&self, most: u64) -> Result<Vec<u8>, StoreError> {
        let mut out = Vec::with_capacity_in(MANIFEST_HEADER + SEGMENT_ROW * self.segments.len() + 24 * self.retired.len() + LEASE_ROW * self.leases.len(), Global);
        out.extend_from_slice(&MANIFEST_MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.retired.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.roots.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.leases.len() as u32).to_le_bytes());
        for (i, it) in self.segments.iter().enumerate() {
            let at = (MANIFEST_HEADER + SEGMENT_ROW * i) as u64;
            let row = Row::new(RowFields::of(it), self.generation, most).map_err(|why| StoreError::Row { generation: self.generation, segment: it.id, at, why })?;
            row.encode_into(&mut out);
        }
        for it in &self.retired {
            out.extend_from_slice(&it.id.to_le_bytes());
            out.extend_from_slice(&u32::from(it.log).to_le_bytes());
            out.extend_from_slice(&LostMark::encode(it.lost).to_le_bytes());
            out.extend_from_slice(&it.since_ns.to_le_bytes());
        }
        for it in &self.leases {
            out.extend_from_slice(&it.holder.to_le_bytes());
            out.extend_from_slice(&it.since.to_le_bytes());
            out.extend_from_slice(&it.opened_ns.to_le_bytes());
            out.extend_from_slice(&it.expires_ns.to_le_bytes());
        }
        for root in &self.roots {
            out.extend_from_slice(&root.key.0);
            out.extend_from_slice(&root.generation.to_le_bytes());
            out.extend_from_slice(&root.expires_ns.to_le_bytes());
            out.extend_from_slice(&(root.name.len() as u16).to_le_bytes());
            out.extend_from_slice(root.name.as_bytes());
            while !out.len().is_multiple_of(8) {
                out.push(0);
            }
        }
        Ok(out)
    }

    /// The manifest `bytes` hold, or why not: [`StoreError::Format`] for one of another format
    /// (format 2's, before retired rows were marked, is refused so, rule 26), a segment row
    /// [`Row::new`] refuses (segments at most `most` bytes) [`StoreError::Row`] naming the
    /// segment, the row's byte and why with its values, else [`StoreError::Manifest`] naming
    /// what is wrong and the byte it is at.
    pub(crate) fn decode(bytes: &[u8], most: u64) -> Result<Manifest, StoreError> {
        let generation = if bytes.len() >= 16 { u64_at(bytes, 8) } else { 0 };
        let bad = |what: &'static str, at: usize| StoreError::Manifest { generation, what, at: at as u64 };
        let short = || bad("it is cut short", bytes.len());
        if bytes.len() < MANIFEST_HEADER {
            return Err(short());
        }
        if bytes[0..4] != MANIFEST_MAGIC {
            return Err(bad("it does not start with `BLPM`: not a plain manifest", 0));
        }
        if u32_at(bytes, 4) != FORMAT {
            return Err(StoreError::Format { what: "a plain store's manifest", found: u32_at(bytes, 4), wanted: FORMAT });
        }
        let (s, r, t, l) = (u32_at(bytes, 16) as usize, u32_at(bytes, 20) as usize, u32_at(bytes, 24) as usize, u32_at(bytes, 28) as usize);
        let rows = MANIFEST_HEADER.saturating_add(SEGMENT_ROW.saturating_mul(s)).saturating_add(24usize.saturating_mul(r));
        let fixed = rows.saturating_add(LEASE_ROW.saturating_mul(l));
        if bytes.len() < fixed {
            return Err(short());
        }
        let mut segments = Vec::with_capacity_in(s, Global);
        for i in 0..s {
            let at = MANIFEST_HEADER + SEGMENT_ROW * i;
            let fields = RowFields::read(bytes, at);
            let segment = u64::from_le_bytes(fields.digest.0[0..8].try_into().expect("8 bytes"));
            let row = Row::new(fields, generation, most).map_err(|why| StoreError::Row { generation, segment, at: at as u64, why })?;
            segments.push(row.segment());
        }
        let mut retired = Vec::with_capacity_in(r, Global);
        for i in 0..r {
            let at = MANIFEST_HEADER + SEGMENT_ROW * s + 24 * i;
            let log = match u32_at(bytes, at + 8) {
                0 => false,
                1 => true,
                _ => return Err(bad("a retired row is neither a segment (0) nor a log (1)", at + 8)),
            };
            let lost = LostMark::decode(u32_at(bytes, at + 12)).map_err(|what| bad(what, at + 12))?;
            if log && lost.is_some() {
                return Err(bad("a retired log is marked lost: only a segment is", at + 12));
            }
            retired.push(Retired { id: u64_at(bytes, at), log, since_ns: u64_at(bytes, at + 16), lost });
        }
        let mut leases = Vec::with_capacity_in(l, Global);
        for i in 0..l {
            let at = rows + LEASE_ROW * i;
            let it = Lease { holder: u64_at(bytes, at), since: u64_at(bytes, at + 8), opened_ns: u64_at(bytes, at + 16), expires_ns: u64_at(bytes, at + 24) };
            if it.since > generation || leases.iter().any(|other: &Lease| other.holder == it.holder) {
                return Err(bad("a lease row is not one a flip writes (its generation newer than the manifest's, or a second lease of one holder)", at));
            }
            leases.push(it);
        }
        let mut roots = Vec::with_capacity_in(t, Global);
        let mut at = fixed;
        for _ in 0..t {
            if bytes.len() < at + 50 {
                return Err(short());
            }
            let key = id_at(bytes, at);
            let generation = u64_at(bytes, at + 32);
            let expires_ns = u64_at(bytes, at + 40);
            let len = u16::from_le_bytes([bytes[at + 48], bytes[at + 49]]) as usize;
            let Some(name) = bytes.get(at + 50..at + 50 + len) else { return Err(short()) };
            let name = std::str::from_utf8(name).map_err(|_| bad("a root's name is not UTF-8", at + 50))?.to_owned();
            roots.push(Root { name, key, generation, expires_ns });
            at = (at + 50 + len).div_ceil(8) * 8;
        }
        if at != bytes.len() {
            return Err(bad("its fields end before its last byte", at));
        }
        Ok(Manifest { generation, segments, roots, leases, retired })
    }

    /// The bytes its segments hold.
    pub(crate) fn bytes(&self) -> u64 {
        self.segments.iter().map(Segment::file_len).sum()
    }
}

/// One index entry, [`ENTRY`] bytes: the key (0..32), the place (32..40, a u64: the payload's
/// offset in units of [`ALIGN`] in bits 0..40, the [`Tag`] in bits 40..56, zero in 56..64; so
/// the tag is the u16 little-endian at bytes 37..39), the payload's length (40..44) and its
/// CRC-32C ([`record_crc`], 44..48).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub key: ContentId,
    pub tag: Tag,
    /// Of its segment; a multiple of [`ALIGN`].
    pub at: u64,
    pub len: u32,
    pub crc: u32,
}

impl Entry {
    pub fn encode_into<A: core::alloc::Allocator>(&self, out: &mut Vec<u8, A>) {
        debug_assert!(self.at.is_multiple_of(ALIGN) && self.at < OFFSET_MOST);
        let place = (self.at / ALIGN) | (u64::from(self.tag.0) << 40);
        out.extend_from_slice(&self.key.0);
        out.extend_from_slice(&place.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.crc.to_le_bytes());
    }

    /// The entry at `bytes[..ENTRY]`, its reserved bits checked (bits 56..64 of its place): what
    /// [`check_segment`] and a ranged probe of an unchecked index use. Why not, naming the rule.
    pub fn decode(bytes: &[u8]) -> Result<Entry, &'static str> {
        let place = u64_at(bytes, 32);
        if place >> 56 != 0 {
            return Err("an entry's place has bits 56..64 set, which no entry of format 2 or 3 sets");
        }
        Ok(Entry::read(bytes))
    }

    /// The entry at `bytes[..ENTRY]` of a proved segment: nothing checked (its segment's one
    /// check covered it, decision 18).
    pub(crate) fn read(bytes: &[u8]) -> Entry {
        let place = u64_at(bytes, 32);
        Entry {
            key: id_at(bytes, 0),
            tag: Tag(((place >> 40) & 0xffff) as u16),
            at: (place & ((1 << 40) - 1)) * ALIGN,
            len: u32_at(bytes, 40),
            crc: u32_at(bytes, 44),
        }
    }

    /// Whether `other` is the same record: what two entries of one key must be.
    pub(crate) fn same_record(&self, other: &Entry) -> bool {
        self.tag == other.tag && self.len == other.len && self.crc == other.crc
    }
}

pub(crate) fn key_at(run: &[u8], i: usize) -> ContentId {
    id_at(run, i * ENTRY)
}

/// The position of `key` in `run`, entries sorted by key: `blaze_format::rows`' lookup, the one
/// home for rows sorted by a digest. Keys are hashes, spread evenly, so it interpolates on their
/// first eight bytes, each guess between the least and most prefix its probes have bounded the
/// range by, then bisects: the probes land on the first guess's page and the key's, where
/// bisection alone takes some twenty over a run of millions, and a run whose keys are not even
/// still costs at most `rows::most_probes(n)`, a binary search's plus `rows::INTERPOLATIONS`.
///
/// No entry is read but the probes (store-next.md): the first guess interpolates over the whole
/// key space (`0`, `u64::MAX`), so a lookup in a cold run reads the pages its probes land on and
/// not the run's first and last (two pages a segment, before).
pub(crate) fn find(run: &[u8], key: &ContentId) -> Option<usize> {
    find_by(run.len() / ENTRY, key, |i| &run[i * ENTRY..i * ENTRY + 32])
}

/// [`find`] over `n` entries whose `i`th key `key_of` reads: the entries a lookup touches are
/// the `i` it asks for, which a test counts as pages.
pub(crate) fn find_by<'a>(n: usize, key: &ContentId, key_of: impl FnMut(usize) -> &'a [u8]) -> Option<usize> {
    rows::seek_from(0, n, 0, &key.0, key_of).ok()
}

/// The positions of `keys` in a run of `n` entries whose `i`th key `key_of` reads, in the
/// order `keys` were given: what [`find_by`] answers for each, found in one pass. The keys are
/// sorted as the run is, and each is sought from where the one before it ended, never before:
/// the run's pages are touched in ascending order, a page the keys share probed once for all of
/// them, where one lookup a key starts each from the whole run (store-next.md, "Batches").
/// Public for the shared cache's reader (blaze-remote's `read`), which seeks a fetched index
/// page of another stride with it (remote-read.md): one interpolation, one home.
pub fn find_many<'a>(n: usize, keys: &[ContentId], mut key_of: impl FnMut(usize) -> &'a [u8]) -> Vec<Option<usize>, Global> {
    // alloc: once per batch, the order its keys are sought in and its answers
    let mut order: Vec<usize, Global> = Vec::with_capacity_in(keys.len(), Global);
    order.extend(0..keys.len());
    order.sort_unstable_by(|a, b| keys[*a].cmp(&keys[*b]));
    let mut found: Vec<Option<usize>, Global> = Vec::with_capacity_in(keys.len(), Global);
    found.resize(keys.len(), None);
    // Each key sought from where the one before it ended: every entry before has a key below.
    let mut seeker = rows::Seeker::new();
    let mut last: Option<(&ContentId, Option<usize>)> = None;
    for at in order {
        let key = &keys[at];
        if let Some((before, answer)) = last
            && before == key
        {
            found[at] = answer;
            continue;
        }
        let answer = seeker.seek(n, &key.0, &mut key_of).ok();
        found[at] = answer;
        last = Some((key, answer));
    }
    found
}

/// `entries` as one sorted run, each key once. Two entries of one key that are not one record
/// (two values under one key, rule 21) stop it with [`StoreError::TwoValues`], naming the key
/// and the later record's tag. The run is the process heap: it is moved whole into the
/// segment's write (blaze-core's effect owns it); `entries` may be in any allocator.
pub(crate) fn sorted_run<B: core::alloc::Allocator>(mut entries: Vec<Entry, B>) -> Result<Vec<u8>, StoreError> {
    entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    let mut out = Vec::with_capacity_in(entries.len() * ENTRY, Global);
    let mut last: Option<Entry> = None;
    for entry in entries {
        match last {
            Some(kept) if kept.key == entry.key => {
                if !kept.same_record(&entry) {
                    // Only the two index entries are at hand: lengths and kinds, no bytes. Both
                    // are committed records (compaction's input; a commit's layout refused
                    // its own two values before it runs this).
                    return Err(StoreError::TwoValues(TwoValues {
                        key: entry.key,
                        tag: entry.tag,
                        len: Some(u64::from(entry.len)),
                        held_tag: kept.tag,
                        held_len: u64::from(kept.len),
                        held: Holder::Committed,
                        differs_at: None,
                    }));
                }
            }
            _ => {
                entry.encode_into(&mut out);
                last = Some(entry);
            }
        }
    }
    Ok(out)
}
