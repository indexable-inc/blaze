//! The spine: the image's header, its section table and the CRCs of its CRC table, all in group 0.
//!
//! Header, [`HEADER`] bytes:
//!
//! | at | field |
//! |---|---|
//! | 0 | magic, [`MAGIC`] |
//! | 8 | [`LAYOUT`] |
//! | 40 | the image's hash: BLAKE3 (domain [`SPINE_DOMAIN`]) of the spine, these 32 bytes zero |
//! | 72 | the file's length, `u64` |
//! | 80 | the spine's length, `u32`: the header, the entries and the top CRCs |
//! | 84 | `group_shift`, `u32` |
//! | 88 | sections, `u32` |
//! | 92 | data groups, `u32` |
//! | 96 | CRC table groups ("tops"), `u32` |
//! | 100 | zero, `u32` |
//! | 104 | where the CRC table starts, `u64` |
//! | 112 | zero, 16 bytes |
//!
//! Then one [`SECTION_ENTRY`]-byte entry a section, in the order of their groups: its id
//! (`u32`), zero (`u32`), its owner's schema (32), where it starts (`u64`), its length
//! (`u64`), its first group (`u32`), its groups (`u32`). Then one `u32` CRC32C a CRC table
//! group.

use blaze_bytes::le::{u32 as u32_at, u64 as u64_at};
use blaze_format::{SchemaHash, schema};
use blaze_ids::{ContentId, Hasher};

use crate::{OpenError, Section, SectionId};

pub const MAGIC: [u8; 8] = *b"BLZIMG1\0";

/// The schema of this layout: a change to the spine, the groups or the CRC table is a new one.
pub const LAYOUT: SchemaHash = schema(
    "blaze image 1: spine in group 0 (header 128: magic 8, layout 32, hash 32, len 8, spine 4, \
     shift 4, sections 4, groups 4, tops 4, 0 4, crc at 8, 0 16; entries 64: id 4, 0 4, schema 32, \
     at 8, len 8, first 4, groups 4; tops u32 each); sections whole groups from group 1, zero \
     padded; CRC32C table last, u32 a data group, whole groups; hash BLAKE3 of the spine",
);

/// The domain of the image's hash.
pub const SPINE_DOMAIN: &str = "blaze v2 image spine 1";

pub const HEADER: usize = 128;
pub const SECTION_ENTRY: usize = 64;
pub const DEFAULT_GROUP_SHIFT: u32 = 16;
/// A page: no group is smaller.
pub const MIN_GROUP_SHIFT: u32 = 12;
pub const MAX_GROUP_SHIFT: u32 = 24;

pub(crate) const HASH_AT: usize = 40;
const LEN_AT: usize = 72;
const SPINE_AT: usize = 80;
const SHIFT_AT: usize = 84;
const SECTIONS_AT: usize = 88;
const GROUPS_AT: usize = 92;
const TOPS_AT: usize = 96;
const CRC_AT: usize = 104;

/// What the header says, after [`parse`] checked it against the file.
#[derive(Clone, Debug)]
pub(crate) struct Spine {
    pub(crate) hash: ContentId,
    pub(crate) len: u64,
    pub(crate) shift: u32,
    pub(crate) groups: u32,
    pub(crate) crc_at: u64,
    pub(crate) sections: Vec<Section>,
    /// The CRC32C of each CRC table group.
    pub(crate) tops: Box<[u32]>,
}

impl Spine {
    pub(crate) fn group_len(&self) -> u64 {
        1 << self.shift
    }
}

/// How many CRC table groups `groups` data groups need: four bytes each.
pub(crate) fn tops_for(groups: u32, shift: u32) -> u32 {
    (u64::from(groups) * 4).div_ceil(1 << shift) as u32
}

pub(crate) fn spine_len(sections: usize, tops: u32) -> usize {
    (HEADER + sections * SECTION_ENTRY + tops as usize * 4).next_multiple_of(8)
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    ContentId(bytes[at..at + 32].try_into().expect("32 bytes"))
}

/// The hash `image`'s spine carries: what `Image::open` of the same bytes gives as
/// [`crate::Image::hash`], for a writer that names the image it just finished
/// ([`crate::Builder::finish`]) without opening it. `None` when the bytes are not an image's:
/// shorter than [`HEADER`], or another magic or layout. It reads the field and checks nothing
/// else: opening checks it.
pub fn hash_of(image: &[u8]) -> Option<ContentId> {
    let header = image.get(..HEADER)?;
    if header[..8] != MAGIC || header[8..40] != LAYOUT.0.0 {
        return None;
    }
    Some(ContentId(header[HASH_AT..HASH_AT + 32].try_into().expect("32 bytes")))
}

/// The hash of `spine`, its hash field read as zero.
pub(crate) fn hash(spine: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(SPINE_DOMAIN);
    hasher.update(&spine[..HASH_AT]).update(&[0; 32]).update(&spine[HASH_AT + 32..]);
    hasher.finish()
}

/// Writes the spine of `spine` into `out`, the first [`spine_len`] bytes of the image, hash
/// last.
pub(crate) fn encode(spine: &Spine, out: &mut [u8]) {
    let len = spine_len(spine.sections.len(), spine.tops.len() as u32);
    let out = &mut out[..len];
    out.fill(0);
    out[..8].copy_from_slice(&MAGIC);
    out[8..40].copy_from_slice(&LAYOUT.0.0);
    out[LEN_AT..LEN_AT + 8].copy_from_slice(&spine.len.to_le_bytes());
    out[SPINE_AT..SPINE_AT + 4].copy_from_slice(&(len as u32).to_le_bytes());
    out[SHIFT_AT..SHIFT_AT + 4].copy_from_slice(&spine.shift.to_le_bytes());
    out[SECTIONS_AT..SECTIONS_AT + 4].copy_from_slice(&(spine.sections.len() as u32).to_le_bytes());
    out[GROUPS_AT..GROUPS_AT + 4].copy_from_slice(&spine.groups.to_le_bytes());
    out[TOPS_AT..TOPS_AT + 4].copy_from_slice(&(spine.tops.len() as u32).to_le_bytes());
    out[CRC_AT..CRC_AT + 8].copy_from_slice(&spine.crc_at.to_le_bytes());
    for (index, section) in spine.sections.iter().enumerate() {
        let entry = &mut out[HEADER + index * SECTION_ENTRY..][..SECTION_ENTRY];
        entry[..4].copy_from_slice(&section.id.0.to_le_bytes());
        entry[8..40].copy_from_slice(&section.schema.0.0);
        entry[40..48].copy_from_slice(&section.at.to_le_bytes());
        entry[48..56].copy_from_slice(&section.len.to_le_bytes());
        entry[56..60].copy_from_slice(&section.first.to_le_bytes());
        entry[60..64].copy_from_slice(&section.groups.to_le_bytes());
    }
    let tops_at = HEADER + spine.sections.len() * SECTION_ENTRY;
    for (index, top) in spine.tops.iter().enumerate() {
        out[tops_at + index * 4..][..4].copy_from_slice(&top.to_le_bytes());
    }
    let hash = hash(out);
    out[HASH_AT..HASH_AT + 32].copy_from_slice(&hash.0);
}

/// The spine of `file`, read from its first `read(len)` bytes only: the header, then the rest of
/// the spine once the header says how long it is. Checks the magic, the layout, the length, the
/// hash, and that the sections tile the groups in order.
pub(crate) fn parse<'a>(
    file_len: u64,
    mut read: impl FnMut(usize) -> &'a [u8],
) -> Result<Spine, OpenError> {
    if file_len < HEADER as u64 {
        return Err(OpenError::Header("shorter than an image's header"));
    }
    let header = read(HEADER);
    if header[..8] != MAGIC {
        return Err(OpenError::Header("a wrong magic"));
    }
    let layout = SchemaHash(id_at(header, 8));
    if layout != LAYOUT {
        return Err(OpenError::Layout { found: layout });
    }
    let len = u64_at(header, LEN_AT);
    if len != file_len {
        return Err(OpenError::Header("a length that is not the file's: a file cut short"));
    }
    let shift = u32_at(header, SHIFT_AT);
    if !(MIN_GROUP_SHIFT..=MAX_GROUP_SHIFT).contains(&shift) {
        return Err(OpenError::Header("a group size out of range"));
    }
    let group = 1u64 << shift;
    let count = u32_at(header, SECTIONS_AT) as usize;
    let groups = u32_at(header, GROUPS_AT);
    let tops = u32_at(header, TOPS_AT);
    let crc_at = u64_at(header, CRC_AT);
    let stated = u32_at(header, SPINE_AT) as usize;
    let wanted_len = spine_len(count, tops);
    if count > group as usize / SECTION_ENTRY || stated != wanted_len || stated as u64 > group {
        return Err(OpenError::Header("a spine longer than its group"));
    }
    if tops != tops_for(groups, shift) {
        return Err(OpenError::Header("a CRC table of the wrong size for its groups"));
    }
    if crc_at != group * (1 + u64::from(groups)) || len != crc_at + u64::from(tops) * group {
        return Err(OpenError::Header("a CRC table that is not where the groups end"));
    }
    let bytes = read(stated);
    let wanted = id_at(bytes, HASH_AT);
    let found = hash(bytes);
    if found != wanted {
        return Err(OpenError::Hash { found, wanted });
    }
    let mut sections = Vec::with_capacity(count);
    let mut next = 0u32;
    for index in 0..count {
        let entry = &bytes[HEADER + index * SECTION_ENTRY..][..SECTION_ENTRY];
        let section = Section {
            id: SectionId(u32_at(entry, 0)),
            schema: SchemaHash(id_at(entry, 8)),
            at: u64_at(entry, 40),
            len: u64_at(entry, 48),
            first: u32_at(entry, 56),
            groups: u32_at(entry, 60),
        };
        let bad = |what| Err(OpenError::Section { index: index as u32, what });
        if sections.iter().any(|it: &Section| it.id == section.id) {
            return bad("a second section of one id");
        }
        if section.first != next || section.groups.checked_add(next).is_none_or(|end| end > groups) {
            return bad("groups that do not follow the section before");
        }
        if section.at != group * (1 + u64::from(section.first))
            || section.len > u64::from(section.groups) * group
            || (section.groups > 0 && section.len <= u64::from(section.groups - 1) * group)
        {
            return bad("a length or place that is not its groups'");
        }
        next += section.groups;
        sections.push(section);
    }
    if next != groups {
        return Err(OpenError::Header("groups no section holds"));
    }
    let tops_at = HEADER + count * SECTION_ENTRY;
    // alloc: the CRC table's top CRCs, once an image opened
    let tops = (0..tops as usize).map(|index| u32_at(bytes, tops_at + index * 4)).collect();
    Ok(Spine { hash: found, len, shift, groups, crc_at, sections, tops })
}
