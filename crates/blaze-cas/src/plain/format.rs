//! The plain store's bytes on disk (the layout is in the module's docs, [`super`]): its head's
//! two slots, a generation's manifest, a segment's index entry, and the pure work over sorted
//! runs of entries (find, merge). Nothing here does IO.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::cmp::Ordering;

use blaze_ids::{ContentId, crc32c};

use crate::{Key, Kind};

/// The format of everything in a plain store's directory. A slot of another format is refused
/// naming both; there is no migration: the store is rebuilt from what made it.
pub(crate) const FORMAT: u32 = 1;

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

/// One index entry's bytes: the key (32), the place (8: offset in 16-byte units, bits 0..40;
/// zero, bits 40..56; kind, bits 56..64), the length (4) and the CRC-32C (4).
pub(crate) const ENTRY: usize = 48;

/// Every payload, run and manifest starts on a multiple of this: `blaze_format`'s alignment,
/// so a record read in place is a buffer that format reads as it lies.
pub(crate) const ALIGN: u64 = blaze_format::ALIGN as u64;

/// The longest offset a place holds: 2^40 units of [`ALIGN`] (16 TiB a segment).
pub(crate) const OFFSET_MOST: u64 = (1 << 40) * ALIGN;

/// The manifest's fixed part.
const MANIFEST_HEADER: usize = 32;

/// One segment's row in a manifest.
const SEGMENT_ROW: usize = 80;

/// `at` rounded up to [`ALIGN`].
pub(crate) const fn aligned(at: u64) -> u64 {
    at.div_ceil(ALIGN) * ALIGN
}

/// The CRC-32C an entry holds: of its key, its kind's byte and its payload, so an entry that
/// points at another record's bytes, or names another kind, fails it.
pub(crate) fn record_crc(key: &Key, kind: Kind, payload: &[u8]) -> u32 {
    let crc = crc32c::extend(crc32c::crc32c(&key.0.0), &[kind.code()]);
    crc32c::extend(crc, payload)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
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
    Foreign { magic: [u8; 4] },
    /// This layout's, whole, of another format.
    Format { found: u32 },
    Slot(Slot),
}

impl Slot {
    /// Bytes 0..4 magic `BLPH`, 4..8 format, 8..16 generation, 16..24 manifest log id, 24..32
    /// manifest offset, 32..36 manifest length, 36..40 manifest CRC-32C, 40..60 zero, 60..64
    /// CRC-32C of bytes 0..60. Little-endian.
    pub(crate) fn encode(&self) -> [u8; SLOT] {
        let mut out = [0u8; SLOT];
        out[0..4].copy_from_slice(&HEAD_MAGIC);
        out[4..8].copy_from_slice(&FORMAT.to_le_bytes());
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
            return SlotRead::Foreign { magic };
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
    /// Not a head this blaze reads; why, without the file's name (the caller adds it).
    Refused(String),
}

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
        let mut magic = [0u8; 4];
        magic[..n].copy_from_slice(&present[..n]);
        return SlotRead::Foreign { magic };
    }
    SlotRead::Torn
}

/// The newest whole slot of `bytes`, the head file as read (empty when it is not there). A
/// head shorter than [`HEAD_LEN`] is read as far as it goes: its missing slots are torn. It is
/// another store's file only when its bytes say so (another magic where a slot begins), never
/// for its length alone, except a file longer than any plain head.
pub(crate) fn read_head(bytes: &[u8]) -> HeadRead {
    if bytes.len() > HEAD_LEN {
        // alloc: a refusal
        return HeadRead::Refused(format!(
            "it is {} bytes, and a plain store's head of format {FORMAT} is never longer than {HEAD_LEN}: it is another store's file (the content-addressed store `blaze_cas::Store` keeps a `head` of its own); give the plain store a directory of its own",
            bytes.len()
        ));
    }
    let slots = [slot_in(bytes, SLOT_AT[0] as usize), slot_in(bytes, SLOT_AT[1] as usize)];
    let mut whole: [Option<Slot>; 2] = [None, None];
    let mut torn = None;
    for (at, read) in slots.iter().enumerate() {
        match *read {
            SlotRead::Blank => {}
            SlotRead::Torn => torn = Some(at),
            SlotRead::Foreign { magic } => {
                // alloc: a refusal
                return HeadRead::Refused(format!(
                    "its slot {at} starts with {magic:02x?}, not a plain store's {HEAD_MAGIC:02x?}: it is another store's file (the content-addressed store `blaze_cas::Store` keeps a `head` of its own); give the plain store a directory of its own"
                ));
            }
            SlotRead::Format { found } => {
                // alloc: a refusal
                return HeadRead::Refused(format!(
                    "its slot {at} is of format {found}, and this blaze reads format {FORMAT}: a plain store is rebuilt from what made it, never migrated; remove the directory"
                ));
            }
            SlotRead::Slot(slot) => {
                if slot.generation % 2 != at as u64 {
                    // alloc: a refusal
                    return HeadRead::Refused(format!(
                        "its slot {at} holds generation {}, which belongs in slot {}",
                        slot.generation,
                        slot.generation % 2
                    ));
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

/// A namespace: which readers a segment is for. Every segment is of exactly one; a lookup
/// names the namespaces it asks, and no other is searched. [`Namespace::PUBLIC`] is std,
/// crates.io packages and what every reader may read; an organisation's is the hash of its
/// name ([`Namespace::named`]); [`Namespace::LOCAL`] is what this machine built for itself and
/// never leaves it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Namespace(pub u64);

impl Namespace {
    pub const PUBLIC: Namespace = Namespace(0);
    pub const LOCAL: Namespace = Namespace(1);

    /// An organisation's: the first eight bytes of a hash of its name (never 0 or 1).
    pub fn named(name: &str) -> Namespace {
        Namespace(id_of("blaze plain store namespace 1", &[name.as_bytes()]).max(2))
    }
}

/// What a segment's header starts with.
pub(crate) const SEGMENT_MAGIC: [u8; 4] = *b"BLPS";

/// What a segment's footer starts with.
pub(crate) const FOOTER_MAGIC: [u8; 4] = *b"BLPE";

/// A segment's header: 0..4 `BLPS`, 4..8 format, 8..16 namespace, 16..20 entries, 20..24 zero,
/// 24..32 the index's offset, 32..64 the lowest key, 64..96 the highest, 96..128 zero. The
/// payloads follow at 128.
pub(crate) const HEADER: usize = 128;

/// A segment's footer: 0..4 `BLPE`, 4..8 entries, 8..16 the index's offset, 16..48 the digest
/// ([`SEGMENT_DIGEST`]) of every byte of the file before the digest.
pub(crate) const FOOTER: usize = 48;

/// The BLAKE3 domain of a segment's digest: its name in a bucket, what a signed statement binds.
pub(crate) const SEGMENT_DIGEST: &str = "blaze plain store segment 1";

/// The most a fetched segment may be: a source's claim past it is refused before a byte is read.
pub const SEGMENT_MOST: u64 = 4 << 30;

/// What a segment's header and footer say, read and checked ([`check_segment`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Described {
    pub namespace: Namespace,
    pub count: u32,
    pub index_at: u64,
    pub digest: ContentId,
}

/// The digest a segment's footer holds: of `bytes[..len - 32]`.
pub(crate) fn segment_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(SEGMENT_DIGEST, &bytes[..bytes.len() - 32])
}

/// A segment's header for `run` (sorted entries) at `index_at` in `namespace`.
pub(crate) fn header(namespace: Namespace, run: &[u8], index_at: u64) -> [u8; HEADER] {
    let count = run.len() / ENTRY;
    let mut out = [0u8; HEADER];
    out[0..4].copy_from_slice(&SEGMENT_MAGIC);
    out[4..8].copy_from_slice(&FORMAT.to_le_bytes());
    out[8..16].copy_from_slice(&namespace.0.to_le_bytes());
    out[16..20].copy_from_slice(&(count as u32).to_le_bytes());
    out[24..32].copy_from_slice(&index_at.to_le_bytes());
    if count > 0 {
        out[32..64].copy_from_slice(&run[0..32]);
        out[64..96].copy_from_slice(&run[(count - 1) * ENTRY..(count - 1) * ENTRY + 32]);
    }
    out
}

/// The footer's first 16 bytes; the digest follows once every byte before it is known.
pub(crate) fn footer_head(count: u32, index_at: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&FOOTER_MAGIC);
    out[4..8].copy_from_slice(&count.to_le_bytes());
    out[8..16].copy_from_slice(&index_at.to_le_bytes());
    out
}

/// The entries a footer says its segment holds, or `None` when the bytes are not a footer.
pub(crate) fn footer_count(bytes: &[u8]) -> Option<u32> {
    (bytes.len() == FOOTER && bytes[0..4] == FOOTER_MAGIC).then(|| u32_at(bytes, 4))
}

/// Checks a whole segment's bytes before anything maps or reads them: its header and footer
/// agree, its length is what they say, its digest is the footer's, and every entry is sorted,
/// of a known kind and inside the payloads. A fetched segment passes this before it is
/// written; a segment of an earlier boot before it is read. Why not, without the file's name.
pub(crate) fn check_segment(bytes: &[u8]) -> Result<Described, String> {
    let len = bytes.len();
    if len < HEADER + FOOTER || len as u64 > SEGMENT_MOST {
        // alloc: a refusal
        return Err(format!("it is {len} bytes: a segment is {} to {SEGMENT_MOST}", HEADER + FOOTER));
    }
    if bytes[0..4] != SEGMENT_MAGIC || u32_at(bytes, 4) != FORMAT {
        // alloc: a refusal
        return Err(format!("its header is {:02x?}, format {}: not a plain segment of format {FORMAT}", &bytes[0..4], u32_at(bytes, 4)));
    }
    let foot = &bytes[len - FOOTER..];
    let (count, index_at) = (u32_at(bytes, 16), u64_at(bytes, 24));
    if foot[0..4] != FOOTER_MAGIC || u32_at(foot, 4) != count || u64_at(foot, 8) != index_at {
        return Err("its footer does not say what its header says: it is cut short or torn".to_owned()); // alloc: a refusal
    }
    let end = index_at.checked_add(u64::from(count) * ENTRY as u64).and_then(|it| it.checked_add(FOOTER as u64));
    if index_at < HEADER as u64 || index_at % ALIGN != 0 || end != Some(len as u64) {
        // alloc: a refusal
        return Err(format!("its index at {index_at} of {count} entries does not end where the file does ({len} bytes)"));
    }
    let digest = ContentId(foot[16..48].try_into().expect("32 bytes"));
    if segment_digest(bytes) != digest {
        return Err("its bytes are not the ones its digest names: torn, cut short or forged".to_owned()); // alloc: a refusal
    }
    let run = &bytes[index_at as usize..index_at as usize + count as usize * ENTRY];
    for i in 0..count as usize {
        let entry = Entry::decode(&run[i * ENTRY..(i + 1) * ENTRY]).map_err(|why| format!("entry {i}: {why}"))?;
        if entry.at < HEADER as u64 || entry.at + u64::from(entry.len) > index_at {
            // alloc: a refusal
            return Err(format!("entry {i} places {} bytes at {}, outside the payloads {HEADER}..{index_at}", entry.len, entry.at));
        }
        if i > 0 && run[(i - 1) * ENTRY..(i - 1) * ENTRY + 32] >= run[i * ENTRY..i * ENTRY + 32] {
            // alloc: a refusal
            return Err(format!("entry {i} is not after entry {}: the index is not sorted", i - 1));
        }
    }
    Ok(Described { namespace: Namespace(u64_at(bytes, 8)), count, index_at, digest })
}

/// What a segment's header and footer say, read by range without its middle (a remote
/// segment's description before anything else of it is fetched); `None` when they are not a
/// plain segment's or disagree. Untrusted until [`check_segment`] passes on the whole.
pub(crate) fn describe(head: &[u8], foot: &[u8], len: u64) -> Option<Described> {
    if head.len() != HEADER || foot.len() != FOOTER || head[0..4] != SEGMENT_MAGIC || u32_at(head, 4) != FORMAT || foot[0..4] != FOOTER_MAGIC {
        return None;
    }
    let (count, index_at) = (u32_at(head, 16), u64_at(head, 24));
    let end = index_at.checked_add(u64::from(count) * ENTRY as u64)?.checked_add(FOOTER as u64)?;
    (u32_at(foot, 4) == count && u64_at(foot, 8) == index_at && end == len).then(|| Described {
        namespace: Namespace(u64_at(head, 8)),
        count,
        index_at,
        digest: ContentId(foot[16..48].try_into().expect("32 bytes")),
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
}

impl Segment {
    pub(crate) fn of(described: &Described, len: u64) -> Segment {
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
        }
    }

    pub(crate) fn file_len(&self) -> u64 {
        self.len
    }

    /// Its sorted run of entries, out of the file's bytes.
    pub(crate) fn run<'a>(&self, file: &'a [u8]) -> &'a [u8] {
        &file[self.index_at as usize..self.index_at as usize + self.count as usize * ENTRY]
    }
}

/// The position of `key` in a sorted run of `count` entries that is read by range (a remote
/// shard, by ranged GET): `read(at, len)` returns those bytes of the run. Interpolation picks a
/// window of [`PROBE_WINDOW`] entries around where the key should be; the key is found or
/// shown absent inside it, else the window's ends narrow the search and the next read is a
/// bisection window. For a shard of a million uniform keys the first window holds the key's
/// place with high likelihood (the guess is off by about `sqrt(n)` = 1000 entries, the window
/// is 1365): one ranged read, two at the tail, at most `2 + log2(n / window)` = 12.
pub(crate) fn find_ranged(count: u32, key: &Key, mut read: impl FnMut(u64, usize) -> Result<Vec<u8>, String>) -> Result<Option<(u32, Entry)>, String> {
    let want = u64::from_be_bytes(key.0.0[0..8].try_into().expect("8 bytes"));
    let (mut lo, mut hi) = (0u64, u64::from(count));
    let (mut lo_prefix, mut hi_prefix) = (0u64, u64::MAX);
    let mut reads = 0;
    while lo < hi {
        let span = hi - lo;
        let window = PROBE_WINDOW.min(span);
        let guess = if reads < 2 {
            lo + ((u128::from(want.saturating_sub(lo_prefix)) * u128::from(span)) / u128::from((hi_prefix - lo_prefix).max(1))) as u64
        } else {
            lo + span / 2
        };
        let start = guess.saturating_sub(window / 2).max(lo).min(hi - window);
        reads += 1;
        let bytes = read(start * ENTRY as u64, (window * ENTRY as u64) as usize)?;
        if bytes.len() != (window * ENTRY as u64) as usize {
            // alloc: a refusal
            return Err(format!("a ranged read of {} entries at {start} returned {} bytes", window, bytes.len()));
        }
        let first = &bytes[0..32];
        let last = &bytes[bytes.len() - ENTRY..bytes.len() - ENTRY + 32];
        if key.0.0[..] < *first {
            hi = start;
            hi_prefix = u64::from_be_bytes(first[0..8].try_into().expect("8 bytes"));
            continue;
        }
        if key.0.0[..] > *last {
            lo = start + window;
            lo_prefix = u64::from_be_bytes(last[0..8].try_into().expect("8 bytes"));
            continue;
        }
        return Ok(match find(&bytes, key) {
            Some(i) => Some((start as u32 + i as u32, Entry::decode(&bytes[i * ENTRY..(i + 1) * ENTRY])?)),
            None => None,
        });
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
    /// The wall clock when it was retired; it is unlinked a grace period later.
    pub since_ns: u64,
}

/// A named root: the record of [`Kind::Pin`] listing the keys compaction keeps for it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Root {
    pub name: String,
    pub key: Key,
    pub generation: u64,
}

/// What one generation holds: its segments (oldest first), its roots, its retired files.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Manifest {
    pub generation: u64,
    pub segments: Vec<Segment>,
    pub roots: Vec<Root>,
    pub retired: Vec<Retired>,
}

impl Manifest {
    /// Bytes 0..4 magic `BLPM`, 4..8 format, 8..16 generation, 16..20 segments `s`, 20..24
    /// retired `r`, 24..28 roots `t`, 28..32 zero; then `s` x 80 (digest 32, length u64, index
    /// offset u64, entries u32, flags u32 (1 here, 2 published), namespace u64, generation
    /// u64, boot u64), `r` x 24 (id u64, 1 for a log u32, zero u32,
    /// since ns u64), and `t` roots, each 32 key, 8 generation, 2 name length, the name's
    /// UTF-8, zeros to a multiple of 8. Little-endian.
    pub(crate) fn encode(&self) -> Vec<u8> {
        // alloc: one a commit
        let mut out = Vec::with_capacity(MANIFEST_HEADER + SEGMENT_ROW * self.segments.len() + 24 * self.retired.len());
        out.extend_from_slice(&MANIFEST_MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.retired.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.roots.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        for it in &self.segments {
            out.extend_from_slice(&it.digest.0);
            out.extend_from_slice(&it.len.to_le_bytes());
            out.extend_from_slice(&it.index_at.to_le_bytes());
            out.extend_from_slice(&it.count.to_le_bytes());
            out.extend_from_slice(&(u32::from(it.local) | u32::from(it.published) << 1).to_le_bytes());
            out.extend_from_slice(&it.namespace.0.to_le_bytes());
            out.extend_from_slice(&it.generation.to_le_bytes());
            out.extend_from_slice(&it.boot.to_le_bytes());
        }
        for it in &self.retired {
            out.extend_from_slice(&it.id.to_le_bytes());
            out.extend_from_slice(&u32::from(it.log).to_le_bytes());
            out.extend_from_slice(&[0u8; 4]);
            out.extend_from_slice(&it.since_ns.to_le_bytes());
        }
        for root in &self.roots {
            out.extend_from_slice(&root.key.0.0);
            out.extend_from_slice(&root.generation.to_le_bytes());
            out.extend_from_slice(&(root.name.len() as u16).to_le_bytes());
            out.extend_from_slice(root.name.as_bytes());
            while out.len() % 8 != 0 {
                out.push(0);
            }
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Manifest, String> {
        // alloc: a refusal
        let short = || format!("it is cut short at {} bytes", bytes.len());
        if bytes.len() < MANIFEST_HEADER {
            return Err(short());
        }
        if bytes[0..4] != MANIFEST_MAGIC {
            // alloc: a refusal
            return Err(format!("it starts with {:02x?}, not {MANIFEST_MAGIC:02x?}", &bytes[0..4]));
        }
        if u32_at(bytes, 4) != FORMAT {
            // alloc: a refusal
            return Err(format!("it is of format {}, not {FORMAT}", u32_at(bytes, 4)));
        }
        let generation = u64_at(bytes, 8);
        let (s, r, t) = (u32_at(bytes, 16) as usize, u32_at(bytes, 20) as usize, u32_at(bytes, 24) as usize);
        let fixed = MANIFEST_HEADER.saturating_add(SEGMENT_ROW.saturating_mul(s)).saturating_add(24usize.saturating_mul(r));
        if bytes.len() < fixed {
            return Err(short());
        }
        // alloc: the manifest, once an open or a commit
        let mut segments = Vec::with_capacity(s);
        for i in 0..s {
            let at = MANIFEST_HEADER + SEGMENT_ROW * i;
            let digest = ContentId(bytes[at..at + 32].try_into().expect("32 bytes"));
            let flags = u32_at(bytes, at + 52);
            let it = Segment {
                id: u64::from_le_bytes(digest.0[0..8].try_into().expect("8 bytes")),
                digest,
                len: u64_at(bytes, at + 32),
                index_at: u64_at(bytes, at + 40),
                count: u32_at(bytes, at + 48),
                local: flags & 1 != 0,
                published: flags & 2 != 0,
                namespace: Namespace(u64_at(bytes, at + 56)),
                generation: u64_at(bytes, at + 64),
                boot: u64_at(bytes, at + 72),
            };
            let end = it.index_at.checked_add(u64::from(it.count) * ENTRY as u64 + FOOTER as u64);
            if it.index_at % ALIGN != 0 || it.len > SEGMENT_MOST || end != Some(it.len) || it.generation > generation || flags > 3 {
                // alloc: a refusal
                return Err(format!("its segment {:016x} of {} bytes, index at {}, generation {}, flags {flags}, is not one a commit writes", it.id, it.len, it.index_at, it.generation));
            }
            segments.push(it);
        }
        // alloc: as above
        let mut retired = Vec::with_capacity(r);
        for i in 0..r {
            let at = MANIFEST_HEADER + SEGMENT_ROW * s + 24 * i;
            retired.push(Retired { id: u64_at(bytes, at), log: u32_at(bytes, at + 8) == 1, since_ns: u64_at(bytes, at + 16) });
        }
        // alloc: as above
        let mut roots = Vec::with_capacity(t);
        let mut at = fixed;
        for _ in 0..t {
            if bytes.len() < at + 42 {
                return Err(short());
            }
            let key = Key(ContentId(bytes[at..at + 32].try_into().expect("32 bytes")));
            let generation = u64_at(bytes, at + 32);
            let len = u16::from_le_bytes([bytes[at + 40], bytes[at + 41]]) as usize;
            let Some(name) = bytes.get(at + 42..at + 42 + len) else { return Err(short()) };
            let name = std::str::from_utf8(name)
                // alloc: a refusal
                .map_err(|_| "a root's name is not UTF-8".to_owned())?
                // alloc: one a root
                .to_owned();
            roots.push(Root { name, key, generation });
            at = (at + 42 + len).div_ceil(8) * 8;
        }
        if at != bytes.len() {
            // alloc: a refusal
            return Err(format!("it is {} bytes and its fields end at {at}", bytes.len()));
        }
        Ok(Manifest { generation, segments, roots, retired })
    }

    /// The bytes its segments hold.
    pub(crate) fn bytes(&self) -> u64 {
        self.segments.iter().map(Segment::file_len).sum()
    }
}

/// One index entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    pub key: Key,
    pub kind: Kind,
    /// Of its segment; a multiple of [`ALIGN`].
    pub at: u64,
    pub len: u32,
    pub crc: u32,
}

impl Entry {
    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        debug_assert!(self.at.is_multiple_of(ALIGN) && self.at < OFFSET_MOST);
        let place = (self.at / ALIGN) | (u64::from(self.kind.code()) << 56);
        out.extend_from_slice(&self.key.0.0);
        out.extend_from_slice(&place.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.crc.to_le_bytes());
    }

    /// The entry at `bytes[..ENTRY]`; an unknown kind's byte is an error naming it.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Entry, String> {
        let key = Key(ContentId(bytes[0..32].try_into().expect("32 bytes")));
        let place = u64_at(bytes, 32);
        let code = (place >> 56) as u8;
        let Some(kind) = Kind::from_code(code) else {
            // alloc: a refusal
            return Err(format!("its kind's byte {code} names no kind"));
        };
        if (place >> 40) & 0xffff != 0 {
            // alloc: a refusal
            return Err(format!("its place {place:016x} has bits set that no entry sets"));
        }
        Ok(Entry { key, kind, at: (place & ((1 << 40) - 1)) * ALIGN, len: u32_at(bytes, 40), crc: u32_at(bytes, 44) })
    }

    /// Whether `other` is the same record: what two entries of one key must be.
    pub(crate) fn same_record(&self, other: &Entry) -> bool {
        self.kind == other.kind && self.len == other.len && self.crc == other.crc
    }
}

pub(crate) fn key_at(run: &[u8], i: usize) -> Key {
    Key(ContentId(run[i * ENTRY..i * ENTRY + 32].try_into().expect("32 bytes")))
}

fn prefix_at(run: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(run[i * ENTRY..i * ENTRY + 8].try_into().expect("8 bytes"))
}

/// The position of `key` in `run`, entries sorted by key. Keys are hashes, spread evenly, so
/// the first steps interpolate on their first eight bytes (about one probe of a page for a
/// run of millions, where bisection takes some twenty); bisection finishes, so a run whose
/// keys are not even still costs at most `3 + log2(n)` probes.
pub(crate) fn find(run: &[u8], key: &Key) -> Option<usize> {
    let want = u64::from_be_bytes(key.0.0[0..8].try_into().expect("8 bytes"));
    let (mut lo, mut hi) = (0usize, run.len() / ENTRY);
    let mut guesses = 0;
    while lo < hi {
        let mid = if guesses < 3 && hi - lo > 8 {
            guesses += 1;
            let (first, last) = (prefix_at(run, lo), prefix_at(run, hi - 1));
            if want < first || want > last {
                return None;
            }
            match last - first {
                0 => lo + (hi - lo) / 2,
                span => lo + ((u128::from(want - first) * (hi - 1 - lo) as u128) / u128::from(span)) as usize,
            }
        } else {
            lo + (hi - lo) / 2
        };
        match run[mid * ENTRY..mid * ENTRY + 32].cmp(&key.0.0[..]) {
            Ordering::Equal => return Some(mid),
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
        }
    }
    None
}

/// `entries` as one sorted run, each key once. Two entries of one key that are not one record
/// (two values under one key, rule 21) stop it with an error naming the key.
pub(crate) fn sorted_run(mut entries: Vec<Entry>) -> Result<Vec<u8>, String> {
    entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    // alloc: the run, written once
    let mut out = Vec::with_capacity(entries.len() * ENTRY);
    let mut last: Option<Entry> = None;
    for entry in entries {
        match last {
            Some(kept) if kept.key == entry.key => {
                if !kept.same_record(&entry) {
                    // alloc: a refusal
                    return Err(format!(
                        "two values under one key {:x}: a {} of {} bytes (CRC {:08x}) and a {} of {} bytes (CRC {:08x}); a key covers every input its value depends on",
                        entry.key.0, kept.kind.name(), kept.len, kept.crc, entry.kind.name(), entry.len, entry.crc
                    ));
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
