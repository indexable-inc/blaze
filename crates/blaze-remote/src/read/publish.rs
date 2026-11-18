//! Sealing a namespace's segments into the objects an origin holds (remote-read.md, "Objects"):
//! each segment is checked whole by the store's own check, then its index (each entry with its
//! value's digest), its summary (filter, fences, page digests), the manifest and the signed
//! head are laid out. Pure: the caller writes the objects (only CI, and later the remote
//! workers, write a shared origin). Every object but the head is named by a digest, so an
//! origin is filled by adding objects and then replacing the head.

use blaze_cas::plain::{Entry, Namespace, check_segment};
use blaze_ids::ContentId;
use blaze_keyed::store::{ENTRY, StoreError};
use blaze_own::{Alloc, Vec};

use super::form::{self, Head, Object, PAGE_ENTRIES_MOST, REMOTE_ENTRY, Row};
use crate::trust::Signer;

/// Why segments were not sealed into an origin's objects: each names the segment.
#[derive(Debug)]
pub enum PublishError {
    /// The store's own check refused it.
    Segment(StoreError),
    Namespace { segment: ContentId, found: Namespace, wanted: Namespace },
    Entry { segment: ContentId, index: u32, what: &'static str },
    TooMany { segments: u64 },
    /// The prefix would be past what any reader takes (`form::PREFIX_MOST`).
    PrefixTooLarge { len: u64 },
}

impl From<StoreError> for PublishError {
    fn from(error: StoreError) -> PublishError {
        PublishError::Segment(error)
    }
}

impl core::fmt::Display for PublishError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let hex = |id: &ContentId| blaze_format::hex::encode(&id.0[..6]);
        match self {
            PublishError::Segment(error) => write!(f, "{error}"),
            PublishError::Namespace { segment, found, wanted } => write!(f, "segment {}: of namespace {:#x}, not {:#x}", hex(segment), found.0, wanted.0),
            PublishError::Entry { segment, index, what } => write!(f, "segment {}, entry {index}: {what}", hex(segment)),
            PublishError::TooMany { segments } => write!(f, "{segments} segments, past the {} a manifest names", form::ROWS_MOST),
            PublishError::PrefixTooLarge { len } => write!(f, "a prefix of {len} bytes, past the {} a reader takes", form::PREFIX_MOST),
        }
    }
}

impl core::error::Error for PublishError {}

/// One object to write: its name's object and its bytes.
pub struct Sealed<'a> {
    pub object: Object,
    pub bytes: Vec<u8, Alloc<'a>>,
}

/// The objects of an origin holding `segments` (whole plain segments of `namespace`), at
/// `generation`, signed by `signer`; the segments themselves are among them, unchanged. Each
/// segment is checked whole first; one that fails, or is of another namespace, stops it by name.
pub fn publish<'a>(namespace: Namespace, generation: u64, signer: &Signer, segments: &[&[u8]], page_entries: u32, alloc: Alloc<'a>) -> Result<Vec<Sealed<'a>, Alloc<'a>>, PublishError> {
    publish_bound(namespace, generation, signer, None, segments, page_entries, alloc)
}

/// [`publish`], the head's signature bound to `bound` when given (a published set's id,
/// blaze-remote published/set.rs `SetName::id`): signed over `bound || head[..HEAD_SIGNED]`, so
/// the head stands only for the set it was made for and a head of another set is refused.
pub fn publish_bound<'a>(
    namespace: Namespace,
    generation: u64,
    signer: &Signer,
    bound: Option<&ContentId>,
    segments: &[&[u8]],
    page_entries: u32,
    alloc: Alloc<'a>,
) -> Result<Vec<Sealed<'a>, Alloc<'a>>, PublishError> {
    assert!((1..=PAGE_ENTRIES_MOST).contains(&page_entries), "entries a page is 1..={PAGE_ENTRIES_MOST}");
    let mut out: Vec<Sealed<'a>, Alloc<'a>> = Vec::new_in(alloc);
    let mut rows: Vec<Row, Alloc<'_>> = Vec::new_in(alloc);
    for &bytes in segments {
        let described = check_segment(bytes)?;
        if described.namespace != namespace {
            return Err(PublishError::Namespace { segment: described.digest, found: described.namespace, wanted: namespace });
        }
        if rows.iter().any(|row| row.segment == described.digest) {
            continue;
        }
        let count = described.count as usize;
        let run = &bytes[described.index_at as usize..described.index_at as usize + count * ENTRY];
        let mut index: Vec<u8, Alloc<'_>> = Vec::with_capacity_in(count * REMOTE_ENTRY, alloc);
        let mut keys: Vec<ContentId, Alloc<'_>> = Vec::with_capacity_in(count, alloc);
        for i in 0..count {
            let raw = &run[i * ENTRY..(i + 1) * ENTRY];
            let entry = Entry::decode(raw).map_err(|what| PublishError::Entry { segment: described.digest, index: i as u32, what })?;
            let payload = &bytes[entry.at as usize..entry.at as usize + entry.len as usize];
            index.extend_from_slice(raw);
            index.extend_from_slice(&crate::segment::value_digest(entry.tag, payload).0);
            keys.push(entry.key);
        }
        let pages = (count as u32).div_ceil(page_entries);
        let mut digests: Vec<ContentId, Alloc<'_>> = Vec::with_capacity_in(pages as usize, alloc);
        for page in 0..pages as usize {
            let from = page * page_entries as usize * REMOTE_ENTRY;
            let to = (from + page_entries as usize * REMOTE_ENTRY).min(index.len());
            digests.push(form::page_digest(&index[from..to]));
        }
        let mut summary = Vec::new_in(alloc);
        form::encode_summary(described.digest, &keys, page_entries, &digests, &mut summary);
        rows.push(Row {
            segment: described.digest,
            len: bytes.len() as u64,
            index_at: described.index_at,
            count: described.count,
            pages,
            page_entries,
            summary: ContentId::of(form::SUMMARY_DOMAIN, &summary),
            summary_len: summary.len() as u64,
        });
        let mut whole = Vec::with_capacity_in(bytes.len(), alloc);
        whole.extend_from_slice(bytes);
        out.push(Sealed { object: Object::Segment(described.digest), bytes: whole });
        out.push(Sealed { object: Object::Index(described.digest), bytes: index });
        out.push(Sealed { object: Object::Summary(described.digest), bytes: summary });
    }
    if rows.len() > form::ROWS_MOST as usize {
        return Err(PublishError::TooMany { segments: rows.len() as u64 });
    }
    rows.sort_unstable_by_key(|row| row.segment);
    let mut manifest = Vec::new_in(alloc);
    form::encode_manifest(namespace, generation, &rows, &mut manifest);
    let digest = ContentId::of(form::MANIFEST_DOMAIN, &manifest);
    let head = Head { namespace, generation, manifest: digest, manifest_len: manifest.len() as u64, signer: signer.public() };
    let head = head.encode(|domain, body| match bound {
        None => signer.sign(domain, body),
        Some(bound) => {
            let mut message = [0u8; 32 + form::HEAD_SIGNED];
            message[..32].copy_from_slice(&bound.0);
            message[32..].copy_from_slice(body);
            signer.sign(domain, &message)
        }
    });
    // The prefix (published.md): the head, the manifest, then each row's summary in row order,
    // the same bytes as the three objects, so each part is checked as they are.
    let mut prefix = Vec::new_in(alloc);
    prefix.extend_from_slice(&head);
    prefix.extend_from_slice(&manifest);
    for row in rows.iter() {
        let summary = out.iter().find(|sealed| sealed.object == Object::Summary(row.segment)).expect("each row's summary was sealed above");
        prefix.extend_from_slice(&summary.bytes);
    }
    if prefix.len() as u64 > form::PREFIX_MOST {
        return Err(PublishError::PrefixTooLarge { len: prefix.len() as u64 });
    }
    out.push(Sealed { object: Object::Manifest(digest), bytes: manifest });
    let mut bytes = Vec::with_capacity_in(head.len(), alloc);
    bytes.extend_from_slice(&head);
    out.push(Sealed { object: Object::Head, bytes });
    out.push(Sealed { object: Object::Prefix, bytes: prefix });
    Ok(out)
}
