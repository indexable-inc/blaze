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
//!
//! What a run of an entry's own text is fed as is the [`Feed`]'s (crate docs, "Identity"): its
//! tokens ([`ByTokens`], what every outline runs), read from the columns of the one lex
//! ([`Lexed`]) with no second pass over the text; or its bytes, for an entry a macro may read the
//! layout of, and for every entry under the planted [`ByBytes`].

use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::{ContentId, Domain, Hasher};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{Item, ItemFlags, ItemIndex, Range};

use crate::Outline;

/// The domain of an unnamed entry's head: its signature's tokens from its first token outside
/// its attributes, which stand for its name among its siblings. `2` since the heads are tokens
/// (`1` hashed the bytes, so a re-spaced `impl` header renamed its members).
static HEAD: Domain = Domain::new("blaze v2 file item head 2");

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
/// The tag of a run of an entry's own tokens in its digest.
const TOKENS: u8 = 2;

/// The tokens of the text an outline is made of, as the walk's columns hold them (crate::walk::rows
/// `Columns`, or the writer's `TokenColumns`): each token's word (its kind,
/// `parser::word_kind`), its start and its end, in order. Whitespace and plain comments are no
/// tokens; a doc comment is one. `words` may be longer than the others (the parser's pad).
#[derive(Clone, Copy)]
pub(crate) struct Lexed<'t> {
    pub(crate) words: &'t [u32],
    pub(crate) starts: &'t [u32],
    pub(crate) ends: &'t [u32],
}

/// What an entry's own text is fed to its digests as: the one seam (AGENTS.md rule 78) between
/// the outline's keys ([`ByTokens`]) and the control that puts trivia back in them ([`ByBytes`]).
/// A head (an unnamed entry's sibling key) is its tokens under either: it names, it holds nothing.
pub trait Feed {
    /// Whether `item`'s own text is fed by its bytes.
    fn by_bytes(item: &Item) -> bool;
}

/// Tokens, but where a macro may read the layout (crate docs, "Identity", rule 21): an entry whose
/// own tokens call a macro or carry an attribute that is no inert built-in one
/// ([`ItemFlags::MACRO_CALL`]) is fed by its bytes, since `stringify!`, `line!`, a proc macro's
/// `Span::source_text` and its spans' lines read what tokens leave out.
pub struct ByTokens;

/// Every entry by its bytes: the planted control (tests/outline/token_keys.rs), the keys as they were
/// before tokens, under which a comment in a body moves its entry's digests.
pub struct ByBytes;

impl Feed for ByTokens {
    fn by_bytes(item: &Item) -> bool {
        item.flags.contains(ItemFlags::MACRO_CALL)
    }
}

impl Feed for ByBytes {
    fn by_bytes(_: &Item) -> bool {
        true
    }
}

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
    /// Its head's file range, its tokens hashed in [`HEAD`]'s domain.
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
/// and whose entries' `hash`es hold their [`Keyed`] makings, `lexed` its tokens: each entry's
/// `disambiguator`, `hash` and `signature_hash` are set, and each top-level entry's sibling key
/// is pushed onto its keys in order. Inputs are hashed side by side by `C` in fixed batches of
/// [`BATCH`], each entry's text fed as `F` says; besides them only `scratch`: the counts of the
/// keys seen, and the messages of one batch.
pub(crate) fn digest<C: Compress, F: Feed>(text: &str, edition: u8, lexed: Lexed<'_>, out: &mut Outline<'_>, scratch: &mut Scratch<'_>) {
    let Scratch { bytes, siblings, held } = scratch;
    bytes.clear();
    siblings.clear();
    let own = Own { bytes: text.as_bytes(), lexed };
    sibling_keys::<C>(own, out, bytes, siblings);
    entry_digests::<C, F>(own, edition, &mut out.items, bytes, held);
    // Last: an entry's names digest may take its `hash` or `signature_hash`.
    crate::names_digest::names_digests::<C>(text, edition, &out.items, &out.attrs, &mut out.names, bytes);
    out.file_names = crate::names_digest::file_names_digest(text, &out.attrs[out.file_attrs.to_usize()]);
}

/// What [`digest`] works in and no outline keeps: the counts of the sibling keys seen, the
/// messages of one batch, and a long entry's [`Buffered`] bytes. Cleared on each use, never
/// freed.
pub(crate) struct Scratch<'a> {
    bytes: AVec<'a, u8>,
    siblings: Map<ContentId, u32, BuildKeyHasher, Alloc<'a>>,
    /// [`HELD`] bytes from the first entry longer than [`SHORT`], empty until then.
    held: AVec<'a, u8>,
}

impl<'a> Scratch<'a> {
    /// Empty: nothing is allocated until the first digest.
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Scratch<'a> {
        Scratch {
            bytes: Vec::new_in(alloc),
            siblings: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: alloc, capacity: None }),
            held: Vec::new_in(alloc),
        }
    }
}

/// Each entry's sibling key and disambiguator: the number of earlier entries with the same
/// key. Entries with one key share a parent, so the count in the order of the entries is the
/// count in source order.
fn sibling_keys<C: Compress>(
    own: Own<'_>,
    out: &mut Outline<'_>,
    scratch: &mut AVec<'_, u8>,
    siblings: &mut Map<ContentId, u32, BuildKeyHasher, Alloc<'_>>,
) {
    let mut from = 0;
    while from < out.items.len() {
        let to = (from + BATCH).min(out.items.len());
        key_batch::<C>(own, out, from..to, scratch, siblings);
        from = to;
    }
}

/// The sibling keys of entries `batch`: their heads' tokens hashed side by side, then their keys.
/// `siblings` counts the keys seen so far.
fn key_batch<C: Compress>(
    own: Own<'_>,
    out: &mut Outline<'_>,
    batch: core::ops::Range<usize>,
    scratch: &mut AVec<'_, u8>,
    siblings: &mut Map<ContentId, u32, BuildKeyHasher, Alloc<'_>>,
) {
    let bytes = own.bytes;
    let mut keyed = [Keyed { tag: 0, by: By::Position }; BATCH];
    for (keyed, item) in keyed.iter_mut().zip(&out.items[batch.clone()]) {
        *keyed = Keyed::unstash(&item.hash);
    }
    let keyed = &keyed[..batch.len()];
    let mut heads = [[0u8; 32]; BATCH];
    {
        // Each head's message, its tokens (a head names: no layout is in it under any feed).
        scratch.clear();
        let mut cuts = [0usize; BATCH + 1];
        let mut count = 0;
        for keyed in keyed {
            if let By::Head(head) = keyed.by {
                tokens(scratch, own, head.start, head.end, head.end);
                count += 1;
                cuts[count] = scratch.len();
            }
        }
        let mut inputs: [&[u8]; BATCH] = [&[]; BATCH];
        for (at, input) in inputs[..count].iter_mut().enumerate() {
            *input = &scratch[cuts[at]..cuts[at + 1]];
        }
        C::derive_many(HEAD.context_key(), &inputs[..count], &mut heads[..count]);
    }
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
fn entry_digests<C: Compress, F: Feed>(
    own: Own<'_>,
    edition: u8,
    items: &mut [Item],
    scratch: &mut AVec<'_, u8>,
    held: &mut AVec<'_, u8>,
) {
    scratch.clear();
    let mut batch = Batch { entries: [0; BATCH], cuts: [Cut { fed: 0, child: 0 }; BATCH], ends: [0; BATCH + 1], count: 0 };
    for at in (0..items.len()).rev() {
        if batch.count > 0 && wide(batch.entries[batch.count - 1]) < items[at].end.at() {
            batch.hash::<C, F>(own, items, scratch);
        }
        if items[at].range.len() > SHORT {
            if held.len() < HELD {
                held.resize(HELD, 0);
            }
            let mut signature = Buffered::of(&SIGNATURE, &mut held[..HELD]);
            let cut = feed_signature::<F>(&mut signature, own, edition, items, at);
            let signature = signature.finish();
            let mut hash = Buffered::of(&ITEM, &mut held[..HELD]);
            feed_rest::<F>(&mut hash, own, items, at, &signature, cut);
            let item = &mut items[at];
            (item.hash, item.signature_hash) = (hash.finish(), signature);
            continue;
        }
        batch.cuts[batch.count] = feed_signature::<F>(scratch, own, edition, items, at);
        batch.entries[batch.count] = narrow(at);
        batch.count += 1;
        batch.ends[batch.count] = scratch.len();
        if batch.count == BATCH || scratch.len() >= BATCH_BYTES {
            batch.hash::<C, F>(own, items, scratch);
        }
    }
    batch.hash::<C, F>(own, items, scratch);
}

/// An outline's text as its digests read it: its bytes and its tokens.
#[derive(Clone, Copy)]
struct Own<'t> {
    bytes: &'t [u8],
    lexed: Lexed<'t>,
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
    fn hash<C: Compress, F: Feed>(&mut self, own: Own<'_>, items: &mut [Item], scratch: &mut AVec<'_, u8>) {
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
            feed_rest::<F>(scratch, own, items, wide(entry), &signature, self.cuts[at]);
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

/// The bytes a [`Buffered`] holds before it feeds its hasher: sixteen chunks, so each feed is
/// whole chunks the hasher compresses side by side (16 lanes on AVX-512, 8 elsewhere).
const HELD: usize = 16 * blaze_ids::b3::CHUNK_LEN;

/// A long entry's message streamed through a [`Hasher`] in feeds of [`HELD`] bytes. The message
/// is many tiny puts (a token's kind, its LEB128 length a byte at a time, its lexeme), and each
/// `Hasher::update` pays the meter's two thread-local adds (blaze-ids `hashed`, D169) and the
/// hasher's own buffering; here a put is a copy into `held`. The digest is the hasher's of the
/// same bytes in the same order: BLAKE3 hashes a stream by its bytes, not by how its updates
/// cut it, so no digest changes (tests/outline.rs `digests_are_the_definition`, which streams
/// each entry of library/core through one hasher a put at a time, holds every long entry to
/// it, and `buffered_is_the_hasher` here holds the sink to the hasher on drawn puts).
struct Buffered<'h> {
    hasher: Hasher,
    /// [`HELD`] bytes, of which `held[..len]` wait to be fed.
    held: &'h mut [u8],
    len: usize,
}

impl<'h> Buffered<'h> {
    /// A message of `domain`, held in `held` ([`HELD`] bytes) until fed.
    fn of(domain: &Domain, held: &'h mut [u8]) -> Buffered<'h> {
        assert_eq!(held.len(), HELD, "a Buffered holds HELD bytes");
        Buffered { hasher: Hasher::of(domain), held, len: 0 }
    }

    /// The digest of every byte put.
    fn finish(mut self) -> ContentId {
        self.feed();
        self.hasher.finish()
    }

    /// The held bytes to the hasher.
    fn feed(&mut self) {
        if self.len > 0 {
            self.hasher.update(&self.held[..self.len]);
            self.len = 0;
        }
    }
}

impl Sink for Buffered<'_> {
    #[inline]
    fn put(&mut self, bytes: &[u8]) {
        let room = HELD - self.len;
        if bytes.len() <= room {
            self.held[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
            return;
        }
        // Fill the held bytes to a whole feed, then feed what is left of `bytes` straight from
        // them when it is a feed or more (a long run of an entry's bytes: no copy), else hold it.
        let (first, rest) = bytes.split_at(room);
        self.held[self.len..].copy_from_slice(first);
        self.len = HELD;
        self.feed();
        if rest.len() >= HELD {
            self.hasher.update(rest);
        } else {
            self.held[..rest.len()].copy_from_slice(rest);
            self.len = rest.len();
        }
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
/// entry's own text up to where the signature ends as tagged runs ([`run`]), with each direct
/// child that starts before the cut as its digest where it stands.
fn feed_signature<F: Feed>(sink: &mut impl Sink, own: Own<'_>, edition: u8, items: &[Item], at: usize) -> Cut {
    let item = &items[at];
    let by_bytes = F::by_bytes(item);
    let signature_end = item.range.start + item.signature.end;
    sink.put(&[edition]);
    let mut fed = item.range.start;
    let mut child = at + 1;
    while child < item.end.at() && items[child].range.start < signature_end {
        let inner = &items[child];
        run(sink, own, by_bytes, fed, inner.range.start, item.range.end);
        sink.put(&[CHILD]);
        sink.put(&inner.hash.0);
        fed = inner.range.end;
        child = inner.end.at();
    }
    assert!(fed <= signature_end, "an entry's signature ends outside its children");
    run(sink, own, by_bytes, fed, signature_end, item.range.end);
    Cut { fed: signature_end, child }
}

/// The message of entry `at` past its signature's `cut`: the signature's digest, then the
/// rest of the entry's own text as runs, with each remaining direct child as its digest.
fn feed_rest<F: Feed>(sink: &mut impl Sink, own: Own<'_>, items: &[Item], at: usize, signature: &ContentId, cut: Cut) {
    let item = &items[at];
    let by_bytes = F::by_bytes(item);
    sink.put(&signature.0);
    let (mut fed, mut child) = (cut.fed, cut.child);
    while child < item.end.at() {
        let inner = &items[child];
        run(sink, own, by_bytes, fed, inner.range.start, item.range.end);
        sink.put(&[CHILD]);
        sink.put(&inner.hash.0);
        fed = inner.range.end;
        child = inner.end.at();
    }
    run(sink, own, by_bytes, fed, item.range.end, item.range.end);
}

/// A run of an entry's own text `from..to`, the entry ending at `limit`: its bytes
/// (`by_bytes`: the tag 0, the length as a u64, the bytes: `Hasher::part`), or its tokens
/// ([`tokens`]).
fn run(sink: &mut impl Sink, own: Own<'_>, by_bytes: bool, from: u32, to: u32, limit: u32) {
    if by_bytes {
        sink.put(&[BYTES]);
        sink.put(&u64::from(to - from).to_le_bytes());
        sink.put(&own.bytes[wide(from)..wide(to)]);
    } else {
        tokens(sink, own, from, to, limit);
    }
}

/// The tokens of the text `from..to` (crate docs, "Identity"): the tag 2, their count (LEB128),
/// then per token its kind (`SyntaxKind` as a little-endian u16) with the top bit set when it is
/// joined to the next token, and, unless it is punctuation (whose kind is its text), its
/// lexeme's length (LEB128) and bytes. Whitespace and plain comments are no tokens; a doc comment
/// is one. A token is joined as `parser::token_key` says (`joined`: punctuation written right
/// against punctuation or a lifetime, what tells `&&` from `& &`), and only to a next token that
/// ends by `limit`, the entry's end, so an entry's digest reads nothing past it. Panics when a
/// token is cut by `from` or `to`: an entry's runs are cut between its tokens, and a token the
/// columns do not hold whole (a body left unlexed) must never go unhashed.
fn tokens(sink: &mut impl Sink, own: Own<'_>, from: u32, to: u32, limit: u32) {
    let Lexed { words, starts, ends } = own.lexed;
    let first = starts.partition_point(|&start| start < from);
    let last = first + starts[first..].partition_point(|&start| start < to);
    assert!(first == 0 || ends[first - 1] <= from, "a token holds byte {from}, where an entry's run is cut");
    sink.put(&[TOKENS]);
    leb128(sink, u64::from(narrow(last - first)));
    for at in first..last {
        let (start, end) = (starts[at], ends[at]);
        assert!(end <= to, "a token from byte {start} to {end} holds byte {to}, where an entry's run is cut");
        let kind = parser::word_kind(words[at]);
        // An outline lexer's body in one token (`LexedStr::new_outline`) is a `{` to the parser
        // and would be hashed as one `{`: what the body says must be its tokens, lexed.
        assert!(
            !(kind.is_punct() && end - start > 3),
            "a {kind:?} from byte {start} to {end} is a body left in one token; an entry's digest is of its tokens, so the text is lexed whole for it"
        );
        let joined = at + 1 < starts.len()
            && starts[at + 1] == end
            && ends[at + 1] <= limit
            && parser::token_key::joined(kind, parser::word_kind(words[at + 1]));
        sink.put(&((kind as u16) | u16::from(joined) << 15).to_le_bytes());
        if !kind.is_punct() {
            leb128(sink, u64::from(end - start));
            sink.put(&own.bytes[wide(start)..wide(end)]);
        }
    }
}

/// `value` as LEB128: seven bits a byte, low first, the top bit set on every byte but the last.
fn leb128(sink: &mut impl Sink, mut value: u64) {
    loop {
        let low = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            sink.put(&[low]);
            return;
        }
        sink.put(&[low | 0x80]);
    }
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

#[cfg(test)]
mod tests {
    use blaze_sim::Config;

    use super::*;

    /// Put lengths where a [`Buffered`] changes path: nothing, a byte, a feed's edges, past two.
    const EDGES: [usize; 6] = [0, 1, HELD - 1, HELD, HELD + 1, 2 * HELD + 5];

    /// A [`Buffered`] fed drawn puts (a token's few bytes, a feed's edges, runs longer than a
    /// feed) finishes to the digest of one [`Hasher`] fed the same puts: the long-entry path's
    /// digests are the streaming definition's whatever the puts.
    #[test]
    fn buffered_is_the_hasher() {
        let data: std::vec::Vec<u8> = (0..4 * HELD).map(|n: usize| n.wrapping_mul(31).to_le_bytes()[0]).collect();
        let mut held = [0u8; HELD];
        blaze_sim::check(Config::cases(if cfg!(miri) { 8 } else { 256 }), |tape| {
            let mut by_hand = Hasher::of(&ITEM);
            let mut buffered = Buffered::of(&ITEM, &mut held);
            let mut at = 0;
            while at < data.len() && tape.more(16, "another put") {
                let len = match tape.below(3, "put kind") {
                    0 => tape.index(9, "a token's"),
                    1 => *tape.pick(&EDGES, "edge").expect("EDGES is not empty"),
                    _ => tape.index(3 * HELD, "any"),
                }
                .min(data.len() - at);
                let put = &data[at..at + len];
                by_hand.update(put);
                buffered.put(put);
                at += len;
            }
            let want = ContentId::of_domain(&ITEM, &data[..at]);
            assert_eq!(by_hand.finish(), want, "{at} bytes put, streamed against one shot");
            assert_eq!(buffered.finish(), want, "{at} bytes put, buffered against one shot");
        });
    }

    /// Totals a byte either side of 1, 2, 4, 8, 16 and 32 chunks (and on them), put as a
    /// token's few bytes at a time, as a block or so, or as whole feeds: the buffered digest,
    /// the plain streamed one and the one-shot digest of the same bytes (blaze-blake3's tree,
    /// another path than its hasher's lazily merged stack) are one value.
    #[test]
    fn buffered_across_chunk_edges_is_one_shot() {
        let chunk = blaze_ids::b3::CHUNK_LEN;
        let data: std::vec::Vec<u8> = (0..32 * chunk + 1).map(|n: usize| n.wrapping_mul(131).to_le_bytes()[0]).collect();
        let mut held = [0u8; HELD];
        for chunks in [1, 2, 4, 8, 16, 32] {
            for total in [chunks * chunk - 1, chunks * chunk, chunks * chunk + 1] {
                if cfg!(miri) && total > 3 * chunk {
                    continue;
                }
                let want = ContentId::of_domain(&ITEM, &data[..total]);
                for piece in [3, 7, 64, 100, chunk + 5, HELD, HELD + 1] {
                    let mut by_hand = Hasher::of(&ITEM);
                    let mut buffered = Buffered::of(&ITEM, &mut held);
                    for put in data[..total].chunks(piece) {
                        by_hand.update(put);
                        buffered.put(put);
                    }
                    assert_eq!(by_hand.finish(), want, "{total} B in puts of {piece}, streamed");
                    assert_eq!(buffered.finish(), want, "{total} B in puts of {piece}, buffered");
                }
            }
        }
    }
}
