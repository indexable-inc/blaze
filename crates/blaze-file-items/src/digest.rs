//! An outline's digests (crate docs, "Identity"), taken when the walk has closed every entry:
//! each entry's sibling key (and from it its disambiguator), its digest and its signature's.
//!
//! The walk records what each key is made of as it closes an entry ([`Keyed`], kept in the
//! entry's `hash` until its digest goes there); nothing is hashed then. At the end the short
//! inputs are hashed side by side, up to [`BATCH`] at a time through [`Compress`], one input a
//! vector lane: the heads, then the sibling keys with the heads' digests in hand, then the
//! entries' digests from the last entry to the first, so that every child's digest is in hand
//! when its parent's message is made. For a batch of entries none of which is another's
//! ancestor, their signatures' messages are hashed side by side, then their messages,
//! each of which starts with its signature's digest. The definition is shaped for this (crate
//! docs, "Identity"): a signature's bytes are compressed once, by the signature's digest, and
//! the entry's digest takes that digest in their place. Messages are copied into a small
//! buffer; an entry longer than a chunk (1 KiB), which no side by side hash speeds up, is
//! streamed through a [`Hasher`] from the file's bytes instead, with no copy. Both ways are
//! [`feed_signature`] and [`feed_rest`], one message definition.
//!
//! The digests are the same bytes whichever [`Compress`] runs ([`SideBySide`] or the twin
//! [`OneByOne`]), and the same as the definition the tests recompute with one streaming hasher
//! per digest.

use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::{ContentId, Domain, Hasher};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{Item, ItemIndex, Range};

use crate::Outline;

/// The domain of an unnamed entry's head: its signature's bytes from its first token outside
/// its attributes, which stand for its name among its siblings.
static HEAD: Domain = Domain::new("blaze v2 file item head 1");

/// The domain of a sibling group's key: the parent, the kind and the name (or head).
static SIBLING: Domain = Domain::new("blaze v2 file item siblings 1");

/// [`ITEM_DOMAIN`](crate::ITEM_DOMAIN), its context key derived once.
static ITEM: Domain = Domain::new(crate::ITEM_DOMAIN);

/// [`SIGNATURE_DOMAIN`](crate::SIGNATURE_DOMAIN), its context key derived once.
static SIGNATURE: Domain = Domain::new(crate::SIGNATURE_DOMAIN);

/// [`PART_DOMAIN`](crate::PART_DOMAIN), its context key derived once.
static PART: Domain = Domain::new(crate::PART_DOMAIN);

/// The tag of a run of an entry's own bytes in its digest.
const BYTES: u8 = 0;
/// The tag of a child entry's digest in its parent's.
const CHILD: u8 = 1;

/// The inputs hashed in one call at most: enough that the lanes left idle at the end of a call
/// (a lane waits for no other, `blaze_blake3::many`) are few beside the steps of the call.
pub(crate) const BATCH: usize = 64;

/// The message bytes one call copies at most: they stay in the second-level cache.
pub(crate) const BATCH_BYTES: usize = 32 * 1024;

/// The longest entry whose message is copied to be hashed side by side: a chunk, BLAKE3's unit
/// of one compression chain. A longer one is hashed alone either way.
const SHORT: u32 = 1024;

/// How a batch of inputs is hashed under one context key: side by side, or one at a time. The
/// one seam between the two (AGENTS.md rule 78); every outline is the same bytes under either.
pub trait Compress {
    /// `blaze_blake3::derive_key_with(key, inputs[i])` into `out[i]`, for each `i`.
    fn derive_many(key: &[u8; 32], inputs: &[&[u8]], out: &mut [[u8; 32]]);
}

/// Inputs side by side, one a SIMD lane (`blaze_blake3::derive_key_many`): what an outline
/// runs.
pub struct SideBySide;

/// Each input by itself (`blaze_blake3::derive_key_with`): the twin [`SideBySide`] is tested
/// equal to.
pub struct OneByOne;

/// The fewest inputs [`SideBySide`] hashes side by side: a call takes as many steps as its
/// longest input has blocks whatever number of lanes are busy, so on 8 lanes one input costs
/// about 4 times its one-shot hash (measured on library/'s entries: 88 ns an input side by side
/// in batches of 64, 168 ns one shot each), and fewer than this many go one at a time. A file
/// outlined part by part has many batches of one or two.
const FEWEST_SIDE_BY_SIDE: usize = 4;

impl Compress for SideBySide {
    fn derive_many(key: &[u8; 32], inputs: &[&[u8]], out: &mut [[u8; 32]]) {
        if inputs.len() < FEWEST_SIDE_BY_SIDE {
            return OneByOne::derive_many(key, inputs, out);
        }
        blaze_blake3::derive_key_many(key, inputs, out);
    }
}

impl Compress for OneByOne {
    fn derive_many(key: &[u8; 32], inputs: &[&[u8]], out: &mut [[u8; 32]]) {
        assert_eq!(inputs.len(), out.len(), "one digest for each input");
        for (input, out) in inputs.iter().zip(out) {
            *out = blaze_blake3::derive_key_with(key, input);
        }
    }
}

/// What an entry's sibling key is made of, besides its parent and kind.
#[derive(Clone, Copy)]
pub(crate) enum By {
    /// Its position alone (a tuple field): nothing more.
    Position,
    /// Its name's file range, `r#` left out, or none for an entry that has none.
    Name(Option<Range>),
    /// Its head's file range, hashed in [`HEAD`]'s domain.
    Head(Range),
}

/// An entry's sibling key's makings besides its parent: its kind as one byte (`walk::tag_of`)
/// and what tells it from its siblings. The walk keeps them in the entry's `hash`, which holds
/// nothing else until [`digest`] takes them out and puts the digest there, so they cost no
/// column of their own ([`Keyed::stash`]).
#[derive(Clone, Copy)]
pub(crate) struct Keyed {
    pub(crate) tag: u8,
    pub(crate) by: By,
}

impl Keyed {
    /// The makings as an entry's `hash` holds them until its digest: the tag, which of
    /// [`By`] (0 a position, 1 no name, 2 a name, 3 a head), and the range's two ends,
    /// little-endian; the rest zero.
    pub(crate) fn stash(self) -> ContentId {
        let (which, range) = match self.by {
            By::Position => (0, Range::new(0, 0)),
            By::Name(None) => (1, Range::new(0, 0)),
            By::Name(Some(name)) => (2, name),
            By::Head(head) => (3, head),
        };
        let mut out = [0u8; 32];
        out[0] = self.tag;
        out[1] = which;
        out[2..6].copy_from_slice(&range.start.to_le_bytes());
        out[6..10].copy_from_slice(&range.end.to_le_bytes());
        ContentId(out)
    }

    /// The makings [`Keyed::stash`] put in `hash`.
    fn unstash(hash: &ContentId) -> Keyed {
        let word = |at: usize| u32::from_le_bytes([hash.0[at], hash.0[at + 1], hash.0[at + 2], hash.0[at + 3]]);
        let range = Range::new(word(2), word(6));
        let by = match hash.0[1] {
            0 => By::Position,
            1 => By::Name(None),
            2 => By::Name(Some(range)),
            3 => By::Head(range),
            other => unreachable!("a stashed sibling key's makings say {other}, which no `By` is"),
        };
        Keyed { tag: hash.0[0], by }
    }
}

/// The digests of `out`, an outline of `text` in `edition` (its hash byte) whose walk is done
/// and whose entries' `hash`es hold their [`Keyed`] makings: each entry's `disambiguator`,
/// `hash` and `signature_hash` are set, and each top-level entry's sibling key is pushed onto
/// its keys in order. Inputs are hashed side by side by `C` in fixed batches of [`BATCH`];
/// besides them only `scratch`: the counts of the keys seen, and the messages of one batch.
pub(crate) fn digest<C: Compress>(text: &str, edition: u8, out: &mut Outline<'_>, scratch: &mut Scratch<'_>) {
    let Scratch { bytes, siblings } = scratch;
    bytes.clear();
    siblings.clear();
    sibling_keys::<C>(text, out, bytes, siblings);
    entry_digests::<C>(text.as_bytes(), edition, &mut out.items, bytes);
    // Last: an entry's names digest may take its `hash` or `signature_hash`.
    crate::names_digest::names_digests::<C>(text, edition, &out.items, &out.attrs, &mut out.names, bytes);
    out.file_names = crate::names_digest::file_names_digest(text, &out.attrs[out.file_attrs.to_usize()]);
}

/// What [`digest`] works in and no outline keeps: the counts of the sibling keys seen, and the
/// messages of one batch. Cleared on each use, never freed.
pub(crate) struct Scratch<'a> {
    bytes: AVec<'a, u8>,
    siblings: Map<ContentId, u32, BuildKeyHasher, Alloc<'a>>,
}

impl<'a> Scratch<'a> {
    /// Empty: nothing is allocated until the first digest.
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Scratch<'a> {
        Scratch { bytes: Vec::new_in(alloc), siblings: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: alloc, capacity: None }) }
    }
}

/// Each entry's sibling key and disambiguator: the number of earlier entries with the same
/// key. Entries with one key share a parent, so the count in the order of the entries is the
/// count in source order.
fn sibling_keys<C: Compress>(
    text: &str,
    out: &mut Outline<'_>,
    scratch: &mut AVec<'_, u8>,
    siblings: &mut Map<ContentId, u32, BuildKeyHasher, Alloc<'_>>,
) {
    let mut from = 0;
    while from < out.items.len() {
        let to = (from + BATCH).min(out.items.len());
        key_batch::<C>(text.as_bytes(), out, from..to, scratch, siblings);
        from = to;
    }
}

/// The sibling keys of entries `batch`: their heads hashed side by side, then their keys.
/// `siblings` counts the keys seen so far.
fn key_batch<C: Compress>(
    bytes: &[u8],
    out: &mut Outline<'_>,
    batch: core::ops::Range<usize>,
    scratch: &mut AVec<'_, u8>,
    siblings: &mut Map<ContentId, u32, BuildKeyHasher, Alloc<'_>>,
) {
    let mut keyed = [Keyed { tag: 0, by: By::Position }; BATCH];
    for (keyed, item) in keyed.iter_mut().zip(&out.items[batch.clone()]) {
        *keyed = Keyed::unstash(&item.hash);
    }
    let keyed = &keyed[..batch.len()];
    let mut inputs: [&[u8]; BATCH] = [&[]; BATCH];
    let mut heads = [[0u8; 32]; BATCH];
    let mut count = 0;
    for keyed in keyed {
        if let By::Head(head) = keyed.by {
            inputs[count] = &bytes[head.to_usize()];
            count += 1;
        }
    }
    C::derive_many(HEAD.context_key(), &inputs[..count], &mut heads[..count]);
    scratch.clear();
    let mut cuts = [0usize; BATCH + 1];
    let mut head = heads.iter();
    for (at, (keyed, item)) in keyed.iter().zip(&out.items[batch.clone()]).enumerate() {
        let parent = item.parent.map_or(0u64, |parent| u64::from(parent.0) + 1);
        scratch.extend_from_slice(&parent.to_le_bytes());
        scratch.push(keyed.tag);
        match keyed.by {
            By::Position => {}
            By::Name(None) => scratch.push(0),
            By::Name(Some(name)) => {
                scratch.push(1);
                scratch.extend_from_slice(&u64::from(name.len()).to_le_bytes());
                scratch.extend_from_slice(&bytes[name.to_usize()]);
            }
            By::Head(_) => scratch.extend_from_slice(head.next().expect("a head's digest for each head")),
        }
        cuts[at + 1] = scratch.len();
    }
    let mut inputs: [&[u8]; BATCH] = [&[]; BATCH];
    for (at, input) in inputs[..batch.len()].iter_mut().enumerate() {
        *input = &scratch[cuts[at]..cuts[at + 1]];
    }
    let mut keys = [[0u8; 32]; BATCH];
    C::derive_many(SIBLING.context_key(), &inputs[..batch.len()], &mut keys[..batch.len()]);
    for (at, key) in batch.zip(keys) {
        let key = ContentId(key);
        let item = &mut out.items[at];
        if item.parent.is_none() {
            out.keys.push((ItemIndex(narrow(at)), key));
        }
        let count = siblings.entry(key).or_insert(0);
        item.disambiguator = *count;
        item.hash = ContentId([0; 32]);
        *count += 1;
    }
}

/// Each entry's digest and signature digest, the entries taken last to first: an entry's
/// descendants come after it, so each child's digest is in hand when its parent's message is
/// made. Short entries wait in a batch, hashed side by side; one whose descendant still waits
/// there has the batch hashed first. In the order taken an entry's descendants are the entries
/// just before it, so the batch holds one exactly when the last entry put in it (the lowest
/// index) is below the entry's end.
fn entry_digests<C: Compress>(bytes: &[u8], edition: u8, items: &mut [Item], scratch: &mut AVec<'_, u8>) {
    scratch.clear();
    let mut batch = Batch { entries: [0; BATCH], cuts: [Cut { fed: 0, child: 0 }; BATCH], ends: [0; BATCH + 1], count: 0 };
    for at in (0..items.len()).rev() {
        if batch.count > 0 && wide(batch.entries[batch.count - 1]) < items[at].end.at() {
            batch.hash::<C>(bytes, items, scratch);
        }
        if items[at].range.len() > SHORT {
            let mut signature = Hasher::of(&SIGNATURE);
            let cut = feed_signature(&mut signature, bytes, edition, items, at);
            let signature = signature.finish();
            let mut hash = Hasher::of(&ITEM);
            feed_rest(&mut hash, bytes, items, at, &signature, cut);
            let item = &mut items[at];
            (item.hash, item.signature_hash) = (hash.finish(), signature);
            continue;
        }
        batch.cuts[batch.count] = feed_signature(scratch, bytes, edition, items, at);
        batch.entries[batch.count] = narrow(at);
        batch.count += 1;
        batch.ends[batch.count] = scratch.len();
        if batch.count == BATCH || scratch.len() >= BATCH_BYTES {
            batch.hash::<C>(bytes, items, scratch);
        }
    }
    batch.hash::<C>(bytes, items, scratch);
}

/// Short entries, none an ancestor of another, whose signatures' messages are copied, one after
/// another in the scratch bytes, waiting to be hashed side by side.
struct Batch {
    entries: [u32; BATCH],
    cuts: [Cut; BATCH],
    /// Where each message ends in the scratch bytes (the first starts at 0).
    ends: [usize; BATCH + 1],
    count: usize,
}

impl Batch {
    /// The waiting entries' signature digests, from their messages in `scratch`, side by side;
    /// then their digests, each message the signature digest and the rest of the entry's, side
    /// by side; and both emptied.
    fn hash<C: Compress>(&mut self, bytes: &[u8], items: &mut [Item], scratch: &mut AVec<'_, u8>) {
        let count = self.count;
        let mut out = [[0u8; 32]; BATCH];
        {
            let mut inputs: [&[u8]; BATCH] = [&[]; BATCH];
            for (at, input) in inputs[..count].iter_mut().enumerate() {
                *input = &scratch[self.ends[at]..self.ends[at + 1]];
            }
            C::derive_many(SIGNATURE.context_key(), &inputs[..count], &mut out[..count]);
        }
        scratch.clear();
        for (at, &entry) in self.entries[..count].iter().enumerate() {
            let signature = ContentId(out[at]);
            items[wide(entry)].signature_hash = signature;
            feed_rest(scratch, bytes, items, wide(entry), &signature, self.cuts[at]);
            self.ends[at + 1] = scratch.len();
        }
        let mut inputs: [&[u8]; BATCH] = [&[]; BATCH];
        for (at, input) in inputs[..count].iter_mut().enumerate() {
            *input = &scratch[self.ends[at]..self.ends[at + 1]];
        }
        C::derive_many(ITEM.context_key(), &inputs[..count], &mut out[..count]);
        for (&entry, digest) in self.entries[..count].iter().zip(out) {
            items[wide(entry)].hash = ContentId(digest);
        }
        self.count = 0;
        scratch.clear();
    }
}

/// Where an entry's message goes: bytes `put` in order. A message is copied for a side by
/// side hash, or streamed through one hasher.
pub(crate) trait Sink {
    fn put(&mut self, bytes: &[u8]);

    /// A length-prefixed part: the bytes `blaze_ids::Hasher::part` hashes.
    fn part(&mut self, bytes: &[u8]) -> &mut Self
    where
        Self: Sized,
    {
        self.put(&(bytes.len() as u64).to_le_bytes());
        self.put(bytes);
        self
    }

    /// A digest's 32 bytes: the bytes `blaze_ids::Hasher::id` hashes.
    fn id(&mut self, id: &ContentId) -> &mut Self
    where
        Self: Sized,
    {
        self.put(&id.0);
        self
    }
}

impl Sink for AVec<'_, u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

impl Sink for Hasher {
    fn put(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

/// Where an entry's message stands at its signature's cut: the next byte of its own and its
/// next direct child.
#[derive(Clone, Copy)]
struct Cut {
    fed: u32,
    child: usize,
}

/// The signature's message of entry `at` (crate docs, "Identity"): the edition, then the
/// entry's own bytes up to where the signature ends as tagged, length-prefixed runs, with each
/// direct child that starts before the cut as its digest where it stands.
fn feed_signature(sink: &mut impl Sink, bytes: &[u8], edition: u8, items: &[Item], at: usize) -> Cut {
    let item = &items[at];
    let signature_end = item.range.start + item.signature.end;
    sink.put(&[edition]);
    let mut fed = item.range.start;
    let mut child = at + 1;
    while child < item.end.at() && items[child].range.start < signature_end {
        let inner = &items[child];
        run(sink, bytes, fed, inner.range.start);
        sink.put(&[CHILD]);
        sink.put(&inner.hash.0);
        fed = inner.range.end;
        child = inner.end.at();
    }
    assert!(fed <= signature_end, "an entry's signature ends outside its children");
    run(sink, bytes, fed, signature_end);
    Cut { fed: signature_end, child }
}

/// The message of entry `at` past its signature's `cut`: the signature's digest, then the
/// rest of the entry's own bytes as runs, with each remaining direct child as its digest.
fn feed_rest(sink: &mut impl Sink, bytes: &[u8], items: &[Item], at: usize, signature: &ContentId, cut: Cut) {
    let item = &items[at];
    sink.put(&signature.0);
    let (mut fed, mut child) = (cut.fed, cut.child);
    while child < item.end.at() {
        let inner = &items[child];
        run(sink, bytes, fed, inner.range.start);
        sink.put(&[CHILD]);
        sink.put(&inner.hash.0);
        fed = inner.range.end;
        child = inner.end.at();
    }
    run(sink, bytes, fed, item.range.end);
}

/// A run of the file's bytes `from..to`: its tag, its length, its bytes (`Hasher::part`).
fn run(sink: &mut impl Sink, bytes: &[u8], from: u32, to: u32) {
    sink.put(&[BYTES]);
    sink.put(&u64::from(to - from).to_le_bytes());
    sink.put(&bytes[wide(from)..wide(to)]);
}

/// The digest of each of `ranges` of `text` in [`PART_DOMAIN`](crate::PART_DOMAIN), into the
/// same place of `out`, hashed `BATCH` side by side: [`part_digest`](crate::part_digest) of
/// each.
pub fn part_digests<C: Compress>(text: &[u8], ranges: &[Range], out: &mut [ContentId]) {
    assert_eq!(ranges.len(), out.len(), "one digest for each part");
    let key = PART.context_key();
    for (ranges, out) in ranges.chunks(BATCH).zip(out.chunks_mut(BATCH)) {
        let mut inputs: [&[u8]; BATCH] = [&[]; BATCH];
        for (input, range) in inputs.iter_mut().zip(ranges) {
            *input = &text[range.to_usize()];
        }
        let mut digests = [[0u8; 32]; BATCH];
        C::derive_many(key, &inputs[..ranges.len()], &mut digests[..ranges.len()]);
        for (out, digest) in out.iter_mut().zip(digests) {
            *out = ContentId(digest);
        }
    }
}

fn wide(value: u32) -> usize {
    usize::try_from(value).expect("a u32 fits usize")
}

fn narrow(value: usize) -> u32 {
    u32::try_from(value).expect("outlines and their entries are counted in u32")
}
