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
const SEGMENT_ROW: usize = 40;

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
    /// No head: nothing was ever committed here.
    Fresh,
    /// The newest whole slot, and the other one when it is whole too: the generation to fall
    /// back to when the newest's manifest did not survive a power cut.
    Committed(Slot, Option<Slot>),
    /// Not a head this blaze reads; why, without the file's name (the caller adds it).
    Refused(String),
}

/// The newest whole slot of `bytes`, the head file as read (empty when it is not there).
pub(crate) fn read_head(bytes: &[u8]) -> HeadRead {
    if bytes.is_empty() {
        return HeadRead::Fresh;
    }
    if bytes.len() != HEAD_LEN {
        // alloc: a refusal
        return HeadRead::Refused(format!(
            "it is {} bytes, and a plain store's head of format {FORMAT} is {HEAD_LEN}: it is another store's file (the content-addressed store `blaze_cas::Store` keeps a `head` of its own); give the plain store a directory of its own",
            bytes.len()
        ));
    }
    let slots = [Slot::decode(&bytes[0..SLOT]), Slot::decode(&bytes[4096..4096 + SLOT])];
    let mut whole: [Option<Slot>; 2] = [None, None];
    for (at, read) in slots.iter().enumerate() {
        match *read {
            SlotRead::Blank | SlotRead::Torn => {}
            SlotRead::Foreign { magic } => {
                // alloc: a refusal
                return HeadRead::Refused(format!(
                    "its slot {at} starts with {magic:02x?}, not a plain store's {HEAD_MAGIC:02x?}: it is another store's file"
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
        Some((newest, older)) => HeadRead::Committed(newest, older),
        // A head is first written whole with its first slot (`Effect::Write`), and a flip only
        // ever writes the slot the newest does not hold: one slot is always whole.
        None => HeadRead::Refused(
            // alloc: a refusal
            "neither of its two slots is whole, and a commit never leaves both torn: it is corrupt".to_owned(),
        ),
    }
}

/// The bytes of a head file holding `slot` alone: the first commit's, written whole.
pub(crate) fn head_with(slot: &Slot) -> Vec<u8> {
    // alloc: the first head, once a store
    let mut out = vec![0u8; HEAD_LEN];
    let at = SLOT_AT[(slot.generation % 2) as usize] as usize;
    out[at..at + SLOT].copy_from_slice(&slot.encode());
    out
}

/// What ends every segment: magic `BLPS`, the entries (u32), the data length (u64). A segment
/// is self-describing, so a file no manifest names is reported with the records it held.
pub(crate) const FOOTER: usize = 16;

/// What a segment's footer starts with.
pub(crate) const SEGMENT_MAGIC: [u8; 4] = *b"BLPS";

pub(crate) fn footer(count: u32, data_len: u64) -> [u8; FOOTER] {
    let mut out = [0u8; FOOTER];
    out[0..4].copy_from_slice(&SEGMENT_MAGIC);
    out[4..8].copy_from_slice(&count.to_le_bytes());
    out[8..16].copy_from_slice(&data_len.to_le_bytes());
    out
}

/// The entries a segment's footer says it holds, or `None` when the bytes are not a footer.
pub(crate) fn footer_count(bytes: &[u8]) -> Option<u32> {
    (bytes.len() == FOOTER && bytes[0..4] == SEGMENT_MAGIC).then(|| u32_at(bytes, 4))
}

/// A segment: one process's commit (or one compaction's output), written once, never changed:
/// its payloads, its sorted run of entries at `data_len`, its footer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Segment {
    pub id: u64,
    pub data_len: u64,
    pub count: u32,
    /// CRC-32C of the whole file.
    pub crc: u32,
    /// The generation that added it: compaction prunes only what is older than a window.
    pub generation: u64,
    /// The machine boot it was written in or last checked whole in ([`super::core`]): a
    /// segment of another boot is checked once before it is read, as a power cut may have lost
    /// what the kernel had not written back.
    pub boot: u64,
}

impl Segment {
    pub(crate) fn file_len(&self) -> u64 {
        self.data_len + u64::from(self.count) * ENTRY as u64 + FOOTER as u64
    }

    /// Its sorted run of entries, out of the file's bytes.
    pub(crate) fn run<'a>(&self, file: &'a [u8]) -> &'a [u8] {
        &file[self.data_len as usize..self.data_len as usize + self.count as usize * ENTRY]
    }
}

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
    /// retired `r`, 24..28 roots `t`, 28..32 zero; then `s` x 40 (id u64, data length u64,
    /// entries u32, CRC u32, generation u64, boot u64), `r` x 24 (id u64, 1 for a log u32, zero u32,
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
            out.extend_from_slice(&it.id.to_le_bytes());
            out.extend_from_slice(&it.data_len.to_le_bytes());
            out.extend_from_slice(&it.count.to_le_bytes());
            out.extend_from_slice(&it.crc.to_le_bytes());
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
            let it = Segment {
                id: u64_at(bytes, at),
                data_len: u64_at(bytes, at + 8),
                count: u32_at(bytes, at + 16),
                crc: u32_at(bytes, at + 20),
                generation: u64_at(bytes, at + 24),
                boot: u64_at(bytes, at + 32),
            };
            if it.data_len % ALIGN != 0 || it.data_len > OFFSET_MOST || it.generation > generation {
                // alloc: a refusal
                return Err(format!("its segment {:016x} holds {} data bytes of generation {}, which no commit writes", it.id, it.data_len, it.generation));
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
