//! A table: a step's value laid out as keyed parts, so that a reader of one key is a reader
//! of that key alone (docs/v2/plan/design/kernel.md, "Parts").
//!
//! - **One record, read in place.** A table is the value's bytes: a header, a directory in the
//!   order of the keys' bytes (where each key and part ends, and the part's digest), the keys,
//!   the parts, and a Merkle index. [`Table::part`] is a binary search of the directory in the
//!   store's own memory: nothing of the table is copied to read one part.
//! - **Three digests.** A part's digest is of its own bytes and nothing else, so a reader of
//!   one key sees no other key. The key set's digest ([`Table::keys_digest`]) is of the keys
//!   and no part. The root ([`Table::digest`]) covers both.
//! - **The index is a function of the entries**, never of how they came to be there (rule 25):
//!   a 16-way trie on the nibbles of each key's hash, where a subtree of at most [`FAN`]
//!   entries is one leaf. A key added or taken out changes the nodes on its own path, so two
//!   tables are compared by walking both from the root and going down only where the digests
//!   differ ([`diff`]): the cost is the changed keys times the depth, with no hash made.
//! - **Deterministic bytes.** [`Builder::finish`] sorts the keys and writes one form: the
//!   same entries given in any order are the same bytes ([`Table::verify`] builds a table
//!   again from its entries and compares).

use blaze_ids::{ContentId, Hasher};

use crate::trie::Cursor;
use crate::{Read, ReadKind, entry, hex};

/// `blaze kernel p`, then the layout's number.
const MAGIC: [u8; 16] = *b"blaze kernel p 1";
const MAGIC_FAMILY: &[u8] = b"blaze kernel p ";
const KEY: &str = "blaze v2 kernel table key 1";
const PART: &str = "blaze v2 kernel table part 1";
const LEAF_KEYS: &str = "blaze v2 kernel table leaf keys 1";
const LEAF_ALL: &str = "blaze v2 kernel table leaf 1";
const BRANCH_KEYS: &str = "blaze v2 kernel table branch keys 1";
const BRANCH_ALL: &str = "blaze v2 kernel table branch 1";
const TAG_LEAF: u8 = 0;
const TAG_BRANCH: u8 = 1;
/// The header: the magic and five lengths.
const HEADER: usize = 16 + 5 * 4;
/// One entry of the directory: where its key ends, where its part ends, its part's digest.
const ENTRY: usize = 4 + 4 + 32;

/// The most entries of a leaf of the index: a subtree of more is a branch.
pub const FAN: usize = 16;
/// The nibbles of a key's hash: no index is deeper.
const DEEPEST: usize = 64;

fn nibble(hash: &ContentId, depth: usize) -> usize {
    let byte = hash.0[depth / 2];
    usize::from(if depth % 2 == 0 { byte >> 4 } else { byte & 0x0f })
}

fn word(bytes: &[u8], at: usize) -> Option<usize> {
    let four: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_be_bytes(four) as usize)
}

fn put_word(out: &mut Vec<u8>, value: usize, what: &str) -> Result<(), String> {
    let value = u32::try_from(value)
        // alloc: an error
        .map_err(|_| format!("a table's {what} ({value}) is past what four bytes count"))?;
    // alloc: the table's bytes
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

/// The part an entry's digest is of: its own, but for a planted bug.
fn digested(at: usize, count: usize) -> usize {
    #[cfg(test)]
    {
        if crate::nest::planted::is(crate::nest::planted::Bug::PartDigest) {
            return (at + 1) % count;
        }
    }
    let _ = count;
    at
}

/// How many digests this thread made building and editing tables: a test's count of the
/// work [`Table::edit`] saves.
#[cfg(test)]
pub(crate) mod hashed {
    use std::cell::Cell;

    thread_local! {
        static HASHED: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn add(count: usize) {
        HASHED.with(|it| it.set(it.get() + count));
    }

    /// The count since the last call, which it sets back to none.
    pub(crate) fn take() -> usize {
        HASHED.with(|it| it.replace(0))
    }
}

/// Counts `count` digests made, in a test.
fn tally(count: usize) {
    #[cfg(test)]
    hashed::add(count);
    let _ = count;
}

/// What a step's `compute` writes a table with: parts in any order, each key once.
#[derive(Default)]
pub struct Builder {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Builder {
    pub fn new() -> Builder {
        // alloc: one run's table, freed once its bytes are made
        Builder { entries: Vec::new() }
    }

    /// Adds the part of `key`.
    pub fn part(&mut self, key: &[u8], part: &[u8]) -> &mut Builder {
        // alloc: one run's table, a key and a part an entry
        self.entries.push((key.to_vec(), part.to_vec()));
        self
    }

    /// The table's bytes. A key given twice is refused, naming it.
    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        self.entries.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(pair) = self.entries.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            // alloc: an error
            return Err(format!("a table was given the key {} twice", hex(&pair[0].0)));
        }
        let entries = &self.entries;
        let count = entries.len();
        tally(2 * count);
        // alloc: one build's hashes, one a key
        let hashes: Vec<ContentId> = entries.iter().map(|(key, _)| ContentId::of(KEY, key)).collect();
        // alloc: one build's digests, one a part
        let digests: Vec<ContentId> = (0..count)
            .map(|at| ContentId::of(PART, &entries[digested(at, count)].1))
            .collect();
        // alloc: one build's entries in the order of their keys' hashes
        let mut order: Vec<u32> = Vec::with_capacity(count);
        for at in 0..count {
            order.push(u32::try_from(at).map_err(|_| {
                // alloc: an error
                format!("a table of {count} entries is past what four bytes count")
            })?);
        }
        order.sort_by(|a, b| hashes[*a as usize].0.cmp(&hashes[*b as usize].0));
        // alloc: one build's index
        let mut nodes = Vec::new();
        let (root, ..) =
            node(&mut nodes, &|at| hashes[at as usize], &|at| digests[at as usize], &mut order, 0)?;
        let keys: usize = entries.iter().map(|(key, _)| key.len()).sum();
        let parts: usize = entries.iter().map(|(_, part)| part.len()).sum();
        // alloc: the table's bytes, once
        let mut out = Vec::with_capacity(HEADER + count * ENTRY + keys + parts + nodes.len());
        out.extend_from_slice(&MAGIC);
        put_word(&mut out, count, "count of entries")?;
        put_word(&mut out, keys, "keys' length")?;
        put_word(&mut out, parts, "parts' length")?;
        put_word(&mut out, nodes.len(), "index's length")?;
        put_word(&mut out, root, "root's place")?;
        let (mut key_end, mut part_end) = (0, 0);
        for ((key, part), digest) in entries.iter().zip(&digests) {
            key_end += key.len();
            part_end += part.len();
            put_word(&mut out, key_end, "keys' length")?;
            put_word(&mut out, part_end, "parts' length")?;
            out.extend_from_slice(&digest.0);
        }
        for (key, _) in entries {
            out.extend_from_slice(key);
        }
        for (_, part) in entries {
            out.extend_from_slice(part);
        }
        out.extend_from_slice(&nodes);
        Ok(out)
    }
}

/// Writes the node over `items` (entries by their place in the order of the keys, here in
/// the order of the keys' hashes) and gives its place, its keys' digest and its digest.
/// Children are written before their parent, so the root is last.
///
/// `hashes` and `digests` give an entry's key's hash and its part's digest by its place.
fn node(
    out: &mut Vec<u8>,
    hashes: &dyn Fn(u32) -> ContentId,
    digests: &dyn Fn(u32) -> ContentId,
    items: &mut [u32],
    depth: usize,
) -> Result<(usize, ContentId, ContentId), String> {
    if items.len() <= FAN || depth == DEEPEST {
        // In the order of the keys: what a comparison of two leaves walks.
        items.sort_unstable();
        tally(2);
        let (mut keys, mut all) = (Hasher::new(LEAF_KEYS), Hasher::new(LEAF_ALL));
        for at in items.iter() {
            keys.id(&hashes(*at));
            all.id(&hashes(*at)).id(&digests(*at));
        }
        let count = u16::try_from(items.len())
            // alloc: an error
            .map_err(|_| format!("{} keys of a table hash alike", items.len()))?;
        let at = out.len();
        // alloc: the index's bytes
        out.push(TAG_LEAF);
        out.extend_from_slice(&keys.finish().0);
        out.extend_from_slice(&all.finish().0);
        out.extend_from_slice(&count.to_be_bytes());
        for item in items.iter() {
            out.extend_from_slice(&item.to_be_bytes());
        }
        return Ok((at, keys.finish(), all.finish()));
    }
    let mut bitmap = 0u16;
    // alloc: one branch's children, at most 16
    let mut children: Vec<(usize, ContentId, ContentId)> = Vec::with_capacity(16);
    let mut rest = items;
    for n in 0..16 {
        let under = rest.iter().take_while(|at| nibble(&hashes(**at), depth) == n).count();
        if under == 0 {
            continue;
        }
        let (these, after) = std::mem::take(&mut rest).split_at_mut(under);
        rest = after;
        bitmap |= 1 << n;
        children.push(node(out, hashes, digests, these, depth + 1)?);
    }
    branch(out, bitmap, &children)
}

/// Writes the branch over `children` (each its place and its two digests, in the order of
/// their nibbles, which `bitmap` says) and gives its place and its two digests.
fn branch(
    out: &mut Vec<u8>,
    bitmap: u16,
    children: &[(usize, ContentId, ContentId)],
) -> Result<(usize, ContentId, ContentId), String> {
    tally(2);
    let (mut keys, mut all) = (Hasher::new(BRANCH_KEYS), Hasher::new(BRANCH_ALL));
    keys.update(&bitmap.to_be_bytes());
    all.update(&bitmap.to_be_bytes());
    for (_, child_keys, child_all) in children {
        keys.id(child_keys);
        all.id(child_all);
    }
    let at = out.len();
    // alloc: the index's bytes
    out.push(TAG_BRANCH);
    out.extend_from_slice(&keys.finish().0);
    out.extend_from_slice(&all.finish().0);
    out.extend_from_slice(&bitmap.to_be_bytes());
    for (child, ..) in children {
        put_word(out, *child, "index's length")?;
    }
    Ok((at, keys.finish(), all.finish()))
}

/// Where an entry of an edited table comes from.
#[derive(Clone, Copy)]
enum Source {
    /// The entry at this place of the table before.
    Before(u32),
    /// The edit of this number.
    Edit(u32),
}

/// One edited key in the index: its hash, its place in the table before where it had one,
/// and its place in the table now where it has one.
struct Touched {
    hash: ContentId,
    before: Option<u32>,
    now: Option<u32>,
}

/// One [`Table::edit`]: the table before, the entries now, and the index being written.
struct Edit<'t, 's> {
    before: &'t Table<'s>,
    /// The place now of each entry of the table before; `u32::MAX` for one that went or
    /// whose part an edit gives.
    places: &'t [u32],
    /// The entries now, in the order of their keys.
    entries: &'t [Source],
    edits: &'t [(&'t [u8], Option<&'t [u8]>)],
    /// The digest of each edit's part.
    digests: &'t [Option<ContentId>],
    out: Vec<u8>,
}

impl<'t> Edit<'t, '_> {
    /// The key, the part and the part's digest of the entry at `place` of the table now.
    fn entry(&self, place: u32) -> Result<(&'t [u8], &'t [u8], ContentId), String> {
        match self.entries.get(place as usize).ok_or_else(torn)? {
            Source::Before(at) => {
                let (key, part) = self.before.entry(*at as usize)?;
                Ok((key, part.bytes, part.digest))
            }
            Source::Edit(n) => {
                let (key, part) = self.edits[*n as usize];
                let digest = self.digests[*n as usize];
                Ok((key, part.ok_or_else(torn)?, digest.ok_or_else(torn)?))
            }
        }
    }

    /// Writes the node now of what the node at `at` of the table before covered (none: a
    /// part of the index that had no entry), `touched` being the edited keys under it in the
    /// order of their hashes: its place and its two digests, or none where no entry is left
    /// under it. A subtree no edit touches is copied with its digests as they are; one an
    /// edit touches is hashed again, down the edited keys' paths only.
    fn rebuild(
        &mut self,
        at: Option<usize>,
        depth: usize,
        touched: &[Touched],
    ) -> Result<Option<(usize, ContentId, ContentId)>, String> {
        if depth > DEEPEST {
            return Err("a table's index is deeper than a key's hash is long".to_owned()); // alloc: an error
        }
        if touched.is_empty() {
            return match at {
                Some(at) => self.copy(at, depth).map(Some),
                None => Ok(None),
            };
        }
        let came = touched.iter().filter(|it| it.before.is_none() && it.now.is_some()).count();
        let went = touched.iter().filter(|it| it.before.is_some() && it.now.is_none()).count();
        if let Some(at) = at
            && let Node::Branch(bitmap, children) = self.before.node(at)?.node
            // It stays a branch where more than `FAN` entries are left under it.
            && self.before.more_than(at, depth, (FAN + went).saturating_sub(came), &mut 0)?
        {
            let mut map = 0u16;
            // alloc: one branch's children, at most 16
            let mut made: Vec<(usize, ContentId, ContentId)> = Vec::with_capacity(16);
            let mut rest = touched;
            for n in 0..16 {
                let under = rest.iter().take_while(|it| nibble(&it.hash, depth) == n).count();
                let (these, after) = rest.split_at(under);
                rest = after;
                let next = child(bitmap, children, n).ok_or_else(torn)?;
                if let Some(child) = self.rebuild(next, depth + 1, these)? {
                    map |= 1 << n;
                    made.push(child);
                }
            }
            return branch(&mut self.out, map, &made).map(Some);
        }
        // A leaf, a branch too few entries are left under, or nothing: the entries now under
        // it are few, and the node over them is made as a build makes it.
        // alloc: one edited subtree's entries
        let mut before: Vec<u32> = Vec::new();
        if let Some(at) = at {
            self.before.under(at, depth, &mut before)?;
        }
        // alloc: one edited subtree's entries, each its place, its key's hash, its digest
        let mut known: Vec<(u32, ContentId, ContentId)> = Vec::with_capacity(before.len() + came);
        let kept = before.iter().map(|at| self.places.get(*at as usize).copied().unwrap_or(u32::MAX));
        for place in kept.filter(|it| *it != u32::MAX).chain(touched.iter().filter_map(|it| it.now)) {
            let (key, _, digest) = self.entry(place)?;
            tally(1);
            known.push((place, ContentId::of(KEY, key), digest));
        }
        if known.is_empty() && depth > 0 {
            return Ok(None);
        }
        // In the order of the keys' hashes: what `node` is given.
        known.sort_by(|a, b| a.1.0.cmp(&b.1.0));
        // alloc: one edited subtree's entries
        let mut items: Vec<u32> = known.iter().map(|it| it.0).collect();
        known.sort_by_key(|it| it.0);
        let at = |place: u32| {
            known.binary_search_by_key(&place, |it| it.0).expect("an entry of this subtree")
        };
        node(&mut self.out, &|place| known[at(place)].1, &|place| known[at(place)].2, &mut items, depth)
            .map(Some)
    }

    /// Writes the subtree at `at` of the table before as it is now: the same digests, each
    /// entry at its place now. Nothing is hashed.
    fn copy(&mut self, at: usize, depth: usize) -> Result<(usize, ContentId, ContentId), String> {
        if depth > DEEPEST {
            return Err("a table's index is deeper than a key's hash is long".to_owned()); // alloc: an error
        }
        let head = self.before.node(at)?;
        match head.node {
            Node::Leaf(places) => {
                let at = self.out.len();
                // alloc: the index's bytes
                self.out.push(TAG_LEAF);
                self.out.extend_from_slice(&head.keys.0);
                self.out.extend_from_slice(&head.all.0);
                self.out.extend_from_slice(&((places.len() / 4) as u16).to_be_bytes());
                for place in places.chunks_exact(4) {
                    let before = u32::from_be_bytes(place.try_into().expect("chunks of four"));
                    let now = self.places.get(before as usize).ok_or_else(torn)?;
                    self.out.extend_from_slice(&now.to_be_bytes());
                }
                Ok((at, head.keys, head.all))
            }
            Node::Branch(bitmap, children) => {
                // alloc: one branch's children, at most 16
                let mut made: Vec<usize> = Vec::with_capacity(16);
                for child in children.chunks_exact(4) {
                    let child = u32::from_be_bytes(child.try_into().expect("chunks of four")) as usize;
                    made.push(self.copy(child, depth + 1)?.0);
                }
                let at = self.out.len();
                self.out.push(TAG_BRANCH);
                self.out.extend_from_slice(&head.keys.0);
                self.out.extend_from_slice(&head.all.0);
                self.out.extend_from_slice(&bitmap.to_be_bytes());
                for child in made {
                    put_word(&mut self.out, child, "index's length")?;
                }
                Ok((at, head.keys, head.all))
            }
        }
    }
}

/// One part of a table, in the table's own bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Part<'s> {
    pub bytes: &'s [u8],
    /// The digest of `bytes`, as the table keeps it: what a read of the part sees.
    pub digest: ContentId,
}

/// A table as the store holds it, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Table<'s> {
    bytes: &'s [u8],
    count: usize,
    directory: &'s [u8],
    keys: &'s [u8],
    parts: &'s [u8],
    nodes: &'s [u8],
    root: usize,
}

enum Node<'s> {
    /// The places of its entries, four bytes each, ascending.
    Leaf(&'s [u8]),
    /// Which nibbles have a child, and the children's places, four bytes each.
    Branch(u16, &'s [u8]),
}

struct Head<'s> {
    keys: ContentId,
    all: ContentId,
    node: Node<'s>,
}

fn torn() -> String {
    // alloc: an error
    "a table's directory or index names bytes outside it".to_owned()
}

fn child(bitmap: u16, children: &[u8], n: usize) -> Option<Option<usize>> {
    if bitmap & (1 << n) == 0 {
        return Some(None);
    }
    let before = (bitmap & ((1u16 << n) - 1)).count_ones() as usize;
    Some(Some(word(children, before * 4)?))
}

impl<'s> Table<'s> {
    /// The table `bytes` are: its header is checked, and nothing else is read. Bytes of
    /// another layout, or that are no table, are refused saying which.
    pub fn read(bytes: &'s [u8]) -> Result<Table<'s>, String> {
        let magic = bytes.get(..16).ok_or("a value shorter than a table's header is no table")?;
        if magic != MAGIC {
            // alloc: an error
            return Err(if magic.starts_with(MAGIC_FAMILY) {
                format!(
                    "a table of another layout (`{}`), where this build reads `{}`",
                    String::from_utf8_lossy(magic),
                    String::from_utf8_lossy(&MAGIC)
                )
            } else {
                format!("not a table: it does not start with `{}`", String::from_utf8_lossy(&MAGIC))
            });
        }
        let short = "a table shorter than its header says";
        let field = |n: usize| word(bytes, 16 + 4 * n).ok_or(short);
        let (count, keys, parts, nodes, root) = (field(0)?, field(1)?, field(2)?, field(3)?, field(4)?);
        let mut input = Cursor::new(&bytes[HEADER..]);
        let directory = count.checked_mul(ENTRY).and_then(|len| input.take(len)).ok_or(short)?;
        let keys = input.take(keys).ok_or(short)?;
        let parts = input.take(parts).ok_or(short)?;
        let nodes = input.take(nodes).ok_or(short)?;
        if !input.ended() {
            return Err("a table longer than its header says".to_owned()); // alloc: an error
        }
        Ok(Table { bytes, count, directory, keys, parts, nodes, root })
    }

    /// Its bytes, whole: what [`Table::read`] was given.
    pub fn bytes(&self) -> &'s [u8] {
        self.bytes
    }

    /// How many keys it has.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The `at`th entry in the order of the keys.
    pub fn entry(&self, at: usize) -> Result<(&'s [u8], Part<'s>), String> {
        self.entry_at(at).ok_or_else(torn)
    }

    fn entry_at(&self, at: usize) -> Option<(&'s [u8], Part<'s>)> {
        let record = self.directory.get(at.checked_mul(ENTRY)?..)?.get(..ENTRY)?;
        let (key_from, part_from) = match at.checked_sub(1) {
            Some(before) => (word(self.directory, before * ENTRY)?, word(self.directory, before * ENTRY + 4)?),
            None => (0, 0),
        };
        let key = self.keys.get(key_from..word(record, 0)?)?;
        let bytes = self.parts.get(part_from..word(record, 4)?)?;
        let digest = ContentId(record[8..].try_into().ok()?);
        Some((key, Part { bytes, digest }))
    }

    /// The part of `key`, none where the table has no such key: a binary search of the
    /// directory, in place.
    pub fn part(&self, key: &[u8]) -> Result<Option<Part<'s>>, String> {
        match self.place(key)? {
            Ok(at) => Ok(Some(self.entry(at)?.1)),
            Err(_) => Ok(None),
        }
    }

    /// The place of `key` in the order of the keys, or the place it would be put at.
    fn place(&self, key: &[u8]) -> Result<Result<usize, usize>, String> {
        let (mut low, mut high) = (0, self.count);
        // Bounded (rule 18): each turn halves what is left.
        while low < high {
            let middle = low + (high - low) / 2;
            match self.entry(middle)?.0.cmp(key) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(Ok(middle)),
            }
        }
        Ok(Err(low))
    }

    /// Whether more than `limit` entries are under the node at `at`: it stops at the first
    /// past the limit. `count` is the entries met so far.
    fn more_than(&self, at: usize, depth: usize, limit: usize, count: &mut usize) -> Result<bool, String> {
        if depth > DEEPEST {
            return Err("a table's index is deeper than a key's hash is long".to_owned()); // alloc: an error
        }
        match self.node(at)?.node {
            Node::Leaf(places) => *count += places.len() / 4,
            Node::Branch(bitmap, children) => {
                for n in 0..16 {
                    if let Some(next) = child(bitmap, children, n).ok_or_else(torn)?
                        && self.more_than(next, depth + 1, limit, count)?
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(*count > limit)
    }

    /// The table with `edits` applied: each a key and its part now, or none for a key that
    /// goes (one the table has not is then no edit). A key given twice is refused, naming it.
    ///
    /// The invariant: the bytes are those [`Builder`] makes of the entries the table then
    /// has (`an_edit_is_the_table_built_again`). What it saves is the digests: only the
    /// edited parts are hashed, and the index's nodes on the edited keys' paths; every other
    /// part's digest and node's digests are copied. The bytes are all written again: a table
    /// is one record.
    pub fn edit(&self, edits: &[(&[u8], Option<&[u8]>)]) -> Result<Vec<u8>, String> {
        // alloc: one edit's keys in their order
        let mut edits: Vec<(&[u8], Option<&[u8]>)> = edits.to_vec();
        edits.sort_by(|a, b| a.0.cmp(b.0));
        if let Some(pair) = edits.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            // alloc: an error
            return Err(format!("a table was given the key {} twice", hex(pair[0].0)));
        }
        let most = u32::try_from(self.count + edits.len())
            // alloc: an error
            .map_err(|_| format!("a table of {} entries is past what four bytes count", self.count))?;
        // alloc: one edit's entries and places, four bytes an entry of the table
        let mut entries: Vec<Source> = Vec::with_capacity(most as usize);
        let mut places: Vec<u32> = vec![u32::MAX; self.count];
        let mut touched: Vec<Touched> = Vec::with_capacity(edits.len());
        let mut digests: Vec<Option<ContentId>> = Vec::with_capacity(edits.len());
        let mut at = 0;
        for (n, (key, part)) in edits.iter().enumerate() {
            let found = self.place(key)?;
            let (Ok(upto) | Err(upto)) = found;
            while at < upto {
                places[at] = entries.len() as u32;
                entries.push(Source::Before(at as u32));
                at += 1;
            }
            if found.is_ok() {
                at = upto + 1;
            }
            let now = part.map(|_| entries.len() as u32);
            if now.is_some() {
                entries.push(Source::Edit(n as u32));
            }
            tally(1 + usize::from(part.is_some()));
            digests.push(part.map(|part| ContentId::of(PART, part)));
            touched.push(Touched { hash: ContentId::of(KEY, key), before: found.ok().map(|it| it as u32), now });
        }
        while at < self.count {
            places[at] = entries.len() as u32;
            entries.push(Source::Before(at as u32));
            at += 1;
        }
        touched.sort_by(|a, b| a.hash.0.cmp(&b.hash.0));
        // alloc: the index now
        let nodes = Vec::with_capacity(self.nodes.len() + 128 * edits.len());
        let mut edit =
            Edit { before: self, places: &places, entries: &entries, edits: &edits, digests: &digests, out: nodes };
        let root = match edit.rebuild(Some(self.root), 0, &touched)? {
            Some((root, ..)) => root,
            None => return Err(torn()),
        };
        let count = entries.len();
        let (mut keys, mut parts) = (0, 0);
        for place in 0..count {
            let (key, part, _) = edit.entry(place as u32)?;
            keys += key.len();
            parts += part.len();
        }
        // alloc: the table's bytes, once
        let mut out = Vec::with_capacity(HEADER + count * ENTRY + keys + parts + edit.out.len());
        out.extend_from_slice(&MAGIC);
        put_word(&mut out, count, "count of entries")?;
        put_word(&mut out, keys, "keys' length")?;
        put_word(&mut out, parts, "parts' length")?;
        put_word(&mut out, edit.out.len(), "index's length")?;
        put_word(&mut out, root, "root's place")?;
        let (mut key_end, mut part_end) = (0, 0);
        for place in 0..count {
            let (key, part, digest) = edit.entry(place as u32)?;
            key_end += key.len();
            part_end += part.len();
            put_word(&mut out, key_end, "keys' length")?;
            put_word(&mut out, part_end, "parts' length")?;
            out.extend_from_slice(&digest.0);
        }
        for place in 0..count {
            out.extend_from_slice(edit.entry(place as u32)?.0);
        }
        for place in 0..count {
            out.extend_from_slice(edit.entry(place as u32)?.1);
        }
        out.extend_from_slice(&edit.out);
        Ok(out)
    }

    fn node(&self, at: usize) -> Result<Head<'s>, String> {
        let read = || {
            let mut input = Cursor::new(self.nodes.get(at..)?);
            let tag = input.u8()?;
            let (keys, all) = (input.id()?, input.id()?);
            let node = match tag {
                TAG_LEAF => {
                    let count = usize::from(input.u16()?);
                    Node::Leaf(input.take(count * 4)?)
                }
                TAG_BRANCH => {
                    let bitmap = input.u16()?;
                    Node::Branch(bitmap, input.take(bitmap.count_ones() as usize * 4)?)
                }
                _ => return None,
            };
            Some(Head { keys, all, node })
        };
        read().ok_or_else(torn)
    }

    /// The digest of its key set: the same for two tables of the same keys, whatever their
    /// parts.
    pub fn keys_digest(&self) -> Result<ContentId, String> {
        Ok(self.node(self.root)?.keys)
    }

    /// The digest of its keys and parts.
    pub fn digest(&self) -> Result<ContentId, String> {
        Ok(self.node(self.root)?.all)
    }

    /// The places of the entries under the node at `at`.
    fn under(&self, at: usize, depth: usize, out: &mut Vec<u32>) -> Result<(), String> {
        if depth > DEEPEST {
            return Err("a table's index is deeper than a key's hash is long".to_owned()); // alloc: an error
        }
        match self.node(at)?.node {
            Node::Leaf(places) => {
                for place in places.chunks_exact(4) {
                    // alloc: one comparison's entries of a subtree that differs
                    out.push(u32::from_be_bytes(place.try_into().expect("chunks of four")));
                }
            }
            Node::Branch(bitmap, children) => {
                for n in 0..16 {
                    if let Some(next) = child(bitmap, children, n).ok_or_else(torn)? {
                        self.under(next, depth + 1, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Checks every byte: the table built again from its own keys and parts is these bytes,
    /// so each digest is of its own part, the keys are sorted and each once, and the index is
    /// the one form of these entries. Linear in the table; a read of a part never runs it.
    pub fn verify(&self) -> Result<(), String> {
        let mut again = Builder::new();
        for at in 0..self.count {
            let (key, part) = self.entry(at)?;
            again.part(key, part.bytes);
        }
        if again.finish()? != self.bytes {
            // alloc: an error
            return Err("a table whose bytes are not the one form of its keys and parts".to_owned());
        }
        Ok(())
    }
}

/// What differs between `old` and `new`: `each` is given every key whose part's digest
/// differs, that `new` has and `old` has not, or that `old` has and `new` has not; the
/// answer is whether the key sets differ. It reads the two indexes from their roots and goes
/// down only where the digests differ, so it costs the keys that differ times the depth.
pub(crate) fn diff<'a>(
    old: &Table<'a>,
    new: &Table<'a>,
    each: &mut dyn FnMut(&'a [u8]) -> Result<(), crate::KernelError>,
) -> Result<Result<bool, crate::KernelError>, String> {
    if let Err(error) = walk(old, Some(old.root), new, Some(new.root), 0, each)? {
        return Ok(Err(error));
    }
    Ok(Ok(old.keys_digest()? != new.keys_digest()?))
}

fn walk<'a>(
    old: &Table<'a>,
    a: Option<usize>,
    new: &Table<'a>,
    b: Option<usize>,
    depth: usize,
    each: &mut dyn FnMut(&'a [u8]) -> Result<(), crate::KernelError>,
) -> Result<Result<(), crate::KernelError>, String> {
    if depth > DEEPEST {
        return Err("a table's index is deeper than a key's hash is long".to_owned()); // alloc: an error
    }
    if let (Some(a), Some(b)) = (a, b) {
        let (a, b) = (old.node(a)?, new.node(b)?);
        if a.all == b.all {
            return Ok(Ok(()));
        }
        if let (Node::Branch(a_bits, a_children), Node::Branch(b_bits, b_children)) = (a.node, b.node) {
            for n in 0..16 {
                let a = child(a_bits, a_children, n).ok_or_else(torn)?;
                let b = child(b_bits, b_children, n).ok_or_else(torn)?;
                if (a.is_some() || b.is_some())
                    && let Err(error) = walk(old, a, new, b, depth + 1, each)?
                {
                    return Ok(Err(error));
                }
            }
            return Ok(Ok(()));
        }
    }
    // A leaf on one side at least: the entries under both, each side in the order of its keys.
    // alloc: one comparison's entries of a subtree that differs
    let (mut was, mut now) = (Vec::new(), Vec::new());
    if let Some(a) = a {
        old.under(a, depth, &mut was)?;
    }
    if let Some(b) = b {
        new.under(b, depth, &mut now)?;
    }
    was.sort_unstable();
    now.sort_unstable();
    let (mut i, mut j) = (0, 0);
    // Bounded (rule 18): each turn passes an entry of one side at least.
    while i < was.len() || j < now.len() {
        let before = match was.get(i) {
            Some(at) => Some(old.entry(*at as usize)?),
            None => None,
        };
        let after = match now.get(j) {
            Some(at) => Some(new.entry(*at as usize)?),
            None => None,
        };
        let order = match (&before, &after) {
            (Some((a, _)), Some((b, _))) => a.cmp(b),
            (Some(_), None) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        let differs = match order {
            std::cmp::Ordering::Less => {
                i += 1;
                before.filter(|_| !missed_removal()).map(|(key, _)| key)
            }
            std::cmp::Ordering::Greater => {
                j += 1;
                after.map(|(key, _)| key)
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
                match (before, after) {
                    (Some((key, a)), Some((_, b))) if a.digest != b.digest => Some(key),
                    _ => None,
                }
            }
        };
        if let Some(key) = differs
            && let Err(error) = each(key)
        {
            return Ok(Err(error));
        }
    }
    Ok(Ok(()))
}

/// Whether a key that went is passed over: never, but for a planted bug.
fn missed_removal() -> bool {
    #[cfg(test)]
    {
        if crate::nest::planted::is(crate::nest::planted::Bug::DiffRemoved) {
            return true;
        }
    }
    false
}

/// What of a step's value a read the kernel answers is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Which<'a> {
    /// The value whole ([`ReadKind::STEP`]).
    Whole,
    /// The part of one key, or that the key has none ([`ReadKind::PART`]).
    Part(&'a [u8]),
    /// Which keys there are ([`ReadKind::KEYS`]).
    Keys,
}

/// The step (its number, then its input: the key of a read of its value) and what of its
/// value a read of one of the kernel's kinds is of. None for a world's read, or a key that is
/// no such read's.
pub(crate) fn split(read: Read<'_>) -> Option<(&[u8], Which<'_>)> {
    match read.kind {
        ReadKind::STEP => Some((read.key, Which::Whole)),
        ReadKind::KEYS | ReadKind::TABLE => Some((read.key, Which::Keys)),
        ReadKind::PART => {
            let (len, rest) = read.key.split_first_chunk::<2>()?;
            let len = usize::from(u16::from_be_bytes(*len));
            (rest.len() >= len).then(|| (&rest[..len], Which::Part(&rest[len..])))
        }
        _ => None,
    }
}

/// Appends the name of a read of `kind` of the step keyed `step`: for [`ReadKind::PART`] the
/// step's key behind its length (two bytes), then `part`; for any other, the step's key.
pub(crate) fn name(kind: ReadKind, step: &[u8], part: &[u8], out: &mut Vec<u8>) {
    entry::name_kind(kind, out);
    if kind == ReadKind::PART {
        let len = u16::try_from(step.len()).expect("a step read by part has a key of at most 65535 bytes");
        // alloc: the caller's buffer for a name
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(step);
    if kind == ReadKind::PART {
        out.extend_from_slice(part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut builder = Builder::new();
        for (key, part) in entries {
            builder.part(key, part);
        }
        builder.finish().expect("keys given once")
    }

    fn numbered(count: u32, changed: Option<u32>) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..count)
            .map(|n| {
                let part = if changed == Some(n) { b"changed".to_vec() } else { n.to_le_bytes().to_vec() };
                (format!("key {n}").into_bytes(), part)
            })
            .collect()
    }

    fn built(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let borrowed: Vec<(&[u8], &[u8])> = entries.iter().map(|(key, part)| (&key[..], &part[..])).collect();
        of(&borrowed)
    }

    /// The invariant: a table edited is, byte for byte, the table built from the entries it
    /// then has. Checked where an edit changes a part, adds a key and takes one out, over
    /// tables whose index is a leaf, becomes a branch (17 entries), stops being one (16) and
    /// has branches under branches (300), and to and from the empty table.
    #[test]
    fn an_edit_is_the_table_built_again() {
        for count in [0u32, 1, 2, 15, 16, 17, 18, 40, 300] {
            let entries = numbered(count, None);
            let before = built(&entries);
            let table = Table::read(&before).expect("a table");
            let mut cases: Vec<Vec<(Vec<u8>, Option<Vec<u8>>)>> = vec![
                vec![],
                vec![(b"a new key".to_vec(), Some(b"new".to_vec()))],
                vec![(b"a new key".to_vec(), None)],
                (0..20).map(|n| (format!("new {n}").into_bytes(), Some(vec![n]))).collect(),
            ];
            for n in [0, count / 2, count.saturating_sub(1)] {
                if n < count {
                    let key = format!("key {n}").into_bytes();
                    cases.push(vec![(key.clone(), Some(b"changed".to_vec()))]);
                    cases.push(vec![(key.clone(), None)]);
                    cases.push(vec![(key, None), (b"another".to_vec(), Some(vec![]))]);
                }
            }
            cases.push((0..count).step_by(3).map(|n| (format!("key {n}").into_bytes(), None)).collect());
            cases.push((0..count).map(|n| (format!("key {n}").into_bytes(), None)).collect());
            for case in cases {
                let mut now: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = entries.iter().cloned().collect();
                for (key, part) in &case {
                    match part {
                        Some(part) => now.insert(key.clone(), part.clone()),
                        None => now.remove(key),
                    };
                }
                let now: Vec<(Vec<u8>, Vec<u8>)> = now.into_iter().collect();
                let edits: Vec<(&[u8], Option<&[u8]>)> =
                    case.iter().map(|(key, part)| (&key[..], part.as_deref())).collect();
                let edited = table.edit(&edits).expect("an edit of keys given once");
                assert!(edited == built(&now), "a table of {count} entries, edited by {} edits", case.len());
                Table::read(&edited).expect("a table").verify().expect("the one form of its entries");
            }
        }
        let table = built(&numbered(3, None));
        let twice = Table::read(&table).expect("a table").edit(&[(&b"k"[..], None), (&b"k"[..], Some(&b"v"[..]))]);
        assert!(twice.unwrap_err().contains("twice"));
    }

    /// One part of 1000 edited hashes the part, its key and the nodes on its path, where a
    /// build hashes every key, every part and every node.
    #[test]
    fn an_edit_hashes_the_path_of_what_it_edits() {
        let entries = numbered(1000, None);
        hashed::take();
        let before = built(&entries);
        let build = hashed::take();
        let table = Table::read(&before).expect("a table");
        let edited = table.edit(&[(&b"key 437"[..], Some(&b"changed"[..]))]).expect("one edit");
        let edit = hashed::take();
        assert!(edited == built(&numbered(1000, Some(437))));
        blaze_report::outln!("one part of 1000: a build makes {build} digests, an edit {edit}");
        // The part and its key (2), the leaf (its 2 digests and a key's hash for each of its
        // entries, at most 16) and 2 for each branch over it; a build makes 2 an entry and 2
        // a node.
        assert!(build >= 2000 && edit <= 2 + (2 + 16) + 2 * 4, "build {build}, edit {edit}");
    }

    fn differing(old: &[u8], new: &[u8]) -> (Vec<Vec<u8>>, bool) {
        let (old, new) = (Table::read(old).expect("a table"), Table::read(new).expect("a table"));
        let mut keys = Vec::new();
        let key_set = diff(&old, &new, &mut |key| {
            keys.push(key.to_vec());
            Ok(())
        })
        .expect("two whole tables")
        .expect("the test's callback refuses nothing");
        keys.sort();
        (keys, key_set)
    }

    /// The invariant: a table's bytes are a function of its entries, whatever order they were
    /// given in, and every part is read back with the digest of its own bytes.
    #[test]
    fn a_table_is_one_form_of_its_entries_and_reads_each_part_in_place() {
        let entries = numbered(1000, None);
        let bytes = built(&entries);
        let mut reversed = entries.clone();
        reversed.reverse();
        assert_eq!(bytes, built(&reversed));
        let table = Table::read(&bytes).expect("a table");
        table.verify().expect("the one form of its entries");
        assert_eq!(table.len(), 1000);
        for (key, part) in &entries {
            let read = table.part(key).expect("a whole table").expect("a key it was given");
            assert_eq!(read.bytes, &part[..]);
            assert_eq!(read.digest, ContentId::of(PART, part));
            // In place: the part's bytes are bytes of the table.
            assert!(bytes.as_ptr_range().contains(&read.bytes.as_ptr()));
        }
        assert_eq!(table.part(b"key 1000").expect("a whole table"), None);
        assert_eq!(table.part(b"").expect("a whole table"), None);
        let empty = of(&[]);
        let empty = Table::read(&empty).expect("a table");
        assert_eq!((empty.len(), empty.part(b"a").expect("a whole table")), (0, None));
    }

    /// The invariant: the comparison of two tables names exactly the keys whose part
    /// differs, that came or that went, and says the key set differs only when a key came or
    /// went.
    #[test]
    fn a_comparison_names_exactly_the_keys_that_differ() {
        let entries = numbered(1000, None);
        let bytes = built(&entries);
        assert_eq!(differing(&bytes, &bytes), (Vec::new(), false));
        let changed = built(&numbered(1000, Some(437)));
        assert_eq!(differing(&bytes, &changed), (vec![b"key 437".to_vec()], false));
        let (old, new) = (Table::read(&bytes).expect("a table"), Table::read(&changed).expect("a table"));
        assert_eq!(old.keys_digest(), new.keys_digest());
        assert_ne!(old.digest(), new.digest());
        let more = built(&numbered(1001, None));
        assert_eq!(differing(&bytes, &more), (vec![b"key 1000".to_vec()], true));
        assert_eq!(differing(&more, &bytes), (vec![b"key 1000".to_vec()], true));
        // From nothing, and across the size where the root turns from a leaf into a branch.
        for count in [0, 1, FAN as u32, FAN as u32 + 1, 300] {
            let (keys, key_set) = differing(&of(&[]), &built(&numbered(count, None)));
            assert_eq!((keys.len(), key_set), (count as usize, count != 0), "{count} keys from none");
            let (keys, _) = differing(&built(&numbered(count, None)), &built(&numbered(count + 1, Some(0))));
            assert_eq!(keys.len(), if count == 0 { 1 } else { 2 }, "{count} keys, one more and one changed");
        }
    }

    /// The invariant: bytes that are no table of this layout are refused saying which, and a
    /// table cut short is refused, never read as a smaller table.
    #[test]
    fn bytes_that_are_no_table_are_refused_by_name() {
        let bytes = built(&numbered(40, None));
        let mut other = bytes.clone();
        other[15] = b'0';
        let error = Table::read(&other).expect_err("layout 0");
        assert!(error.contains("another layout (`blaze kernel p 0`)"), "{error}");
        let error = Table::read(b"no table at all, but long enough for a header").expect_err("no table");
        assert!(error.contains("not a table"), "{error}");
        let error = Table::read(&bytes[..bytes.len() - 1]).expect_err("cut short");
        assert!(error.contains("shorter than its header says"), "{error}");
        let mut twice = Builder::new();
        twice.part(b"a", b"1").part(b"a", b"2");
        assert!(twice.finish().expect_err("a key twice").contains("the key 61 twice"));
    }
}
