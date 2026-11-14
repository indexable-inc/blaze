//! The index: shards of sorted fixed-width entries under a signed root.
//!
//! An entry, 128 bytes: key 32, value digest 32 ([`crate::segment::value_digest`]), segment
//! digest 32, segment length u64, segment index offset u64, record offset u64, record length
//! u32, segment record count u32. It tells a reader the segment to fetch, and a pusher the
//! value to compare.
//!
//! A shard object: a head, then blocks. The head is 32 bytes (`BLRS`, format u32, namespace
//! u64, entries u32, blocks u32, generation u64) and then per block its first key (32) and
//! digest (32). Each block is 256 entries (32 KiB), the last one shorter. A shard's digest is
//! that of its head. The head holds every block's digest, so one digest commits the whole shard,
//! and one block can be fetched by range and checked alone.
//!
//! A root: an 80-byte header (`BLRR`, format u32, namespace u64, generation u64, key list
//! serial u64, issued u64, runs u32, zero u32, previous root's digest 32), then per run 16
//! bytes (generation u64, shards u32, zero u32) and that run's shard references of 72 bytes
//! (first key 32, digest 32, entries u32, zero u32), newest run first. Last come the signer's
//! key (32) and signature (64). Within a run, shards are disjoint and sorted: shard i holds
//! the keys from its first key up to the next shard's.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;

use crate::{
    trust::{PublicKey, Refusal, Signer, Why, verify_sig},
    wire::{id_at, key_at, put_u32, put_u64, sig_at, u32_at, u64_at},
};

pub const ENTRY: usize = 128;
pub const BLOCK_ENTRIES: usize = 256;
pub const BLOCK: usize = ENTRY * BLOCK_ENTRIES;
pub const BLOCKS_MOST: usize = 256;
/// The most entries a shard holds: 65,536 (8 MiB of blocks).
pub const SHARD_MOST: usize = BLOCK_ENTRIES * BLOCKS_MOST;
const HEAD: usize = 32;
const HEAD_BLOCK: usize = 64;
/// The most bytes a root may be.
pub const ROOT_MOST: u64 = 8 << 20;
/// The most runs a root may hold: a reader refuses more.
pub const RUNS_MOST: usize = 64;
/// Runs kept before the newest two are merged whatever their sizes.
pub const RUNS_KEPT: usize = 8;

const SHARD_DOMAIN: &str = "blaze-remote shard head 1";
const BLOCK_DOMAIN: &str = "blaze-remote shard block 1";
const ROOT_DOMAIN: &str = "blaze-remote root 1";

/// One index entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub key: [u8; 32],
    pub value: ContentId,
    pub segment: ContentId,
    pub segment_len: u64,
    pub index_at: u64,
    pub at: u64,
    pub len: u32,
    pub count: u32,
}

impl Entry {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.key);
        out.extend_from_slice(&self.value.0);
        out.extend_from_slice(&self.segment.0);
        put_u64(out, self.segment_len);
        put_u64(out, self.index_at);
        put_u64(out, self.at);
        put_u32(out, self.len);
        put_u32(out, self.count);
    }

    fn decode(bytes: &[u8]) -> Result<Entry, Why> {
        let entry = Entry {
            key: key_at(bytes, 0),
            value: id_at(bytes, 32),
            segment: id_at(bytes, 64),
            segment_len: u64_at(bytes, 96),
            index_at: u64_at(bytes, 104),
            at: u64_at(bytes, 112),
            len: u32_at(bytes, 120),
            count: u32_at(bytes, 124),
        };
        let fits = entry.at >= 128 && entry.at.checked_add(u64::from(entry.len)).is_some_and(|end| end <= entry.index_at) && entry.index_at.checked_add(u64::from(entry.count) * 48 + 48) == Some(entry.segment_len);
        if !fits || entry.segment_len > blaze_cas::plain::SEGMENT_MOST {
            return Err(Why::OutsideSegment);
        }
        Ok(entry)
    }
}

/// A shard's head length for `count` entries.
pub fn head_len(count: u32) -> usize {
    HEAD + blocks_of(count as usize) * HEAD_BLOCK
}

fn blocks_of(count: usize) -> usize {
    count.div_ceil(BLOCK_ENTRIES)
}

/// A shard's whole length for `count` entries.
pub fn shard_len(count: u32) -> usize {
    head_len(count) + count as usize * ENTRY
}

/// A shard of `entries` (sorted, distinct, 1 to [`SHARD_MOST`]): its digest and its bytes.
pub fn encode(namespace: Namespace, generation: u64, entries: &[Entry]) -> (ContentId, Vec<u8>) {
    assert!(!entries.is_empty() && entries.len() <= SHARD_MOST, "a shard holds 1 to {SHARD_MOST} entries");
    let blocks = blocks_of(entries.len());
    let mut body = Vec::with_capacity(entries.len() * ENTRY);
    for entry in entries {
        entry.encode(&mut body);
    }
    let mut out = Vec::with_capacity(HEAD + blocks * HEAD_BLOCK + body.len());
    out.extend_from_slice(b"BLRS");
    put_u32(&mut out, 1);
    put_u64(&mut out, namespace.0);
    put_u32(&mut out, entries.len() as u32);
    put_u32(&mut out, blocks as u32);
    put_u64(&mut out, generation);
    for (i, block) in body.chunks(BLOCK).enumerate() {
        out.extend_from_slice(&entries[i * BLOCK_ENTRIES].key);
        out.extend_from_slice(&ContentId::of(BLOCK_DOMAIN, block).0);
    }
    let digest = ContentId::of(SHARD_DOMAIN, &out);
    out.extend_from_slice(&body);
    (digest, out)
}

/// A shard's head, checked against the digest the root names.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Head {
    pub count: u32,
    /// Per block: its first key and its digest.
    pub blocks: Vec<([u8; 32], ContentId)>,
}

impl Head {
    pub fn decode(object: &str, bytes: &[u8], shard: &ShardRef, namespace: Namespace) -> Result<Head, Refusal> {
        let refuse = |why| Refusal::new(object, None, why);
        let want = head_len(shard.count);
        if bytes.len() != want {
            return Err(refuse(Why::Length { len: bytes.len() as u64, wanted: want as u64 }));
        }
        crate::trust::note(crate::trust::Check::Digest, shard.digest.0);
        if ContentId::of(SHARD_DOMAIN, bytes) != shard.digest {
            return Err(refuse(Why::Digest));
        }
        if bytes[0..4] != *b"BLRS" || u32_at(bytes, 4) != 1 || u32_at(bytes, 16) != shard.count || u32_at(bytes, 20) as usize != blocks_of(shard.count as usize) {
            return Err(refuse(Why::Malformed("a shard head that does not say what its root says")));
        }
        if u64_at(bytes, 8) != namespace.0 && !crate::planted(crate::Plant::SkipNamespace) {
            return Err(refuse(Why::Namespace { found: u64_at(bytes, 8), wanted: namespace.0 }));
        }
        let blocks = (0..blocks_of(shard.count as usize)).map(|i| (key_at(bytes, HEAD + i * HEAD_BLOCK), id_at(bytes, HEAD + i * HEAD_BLOCK + 32))).collect();
        Ok(Head { count: shard.count, blocks })
    }

    /// Entries in block `i`.
    pub fn block_entries(&self, i: usize) -> usize {
        (self.count as usize - i * BLOCK_ENTRIES).min(BLOCK_ENTRIES)
    }

    /// The block that would hold `key`, if any.
    pub fn block_of(&self, key: &[u8; 32]) -> Option<usize> {
        self.blocks.partition_point(|(first, _)| first <= key).checked_sub(1)
    }

    /// The byte range of block `i` in the shard object.
    pub fn block_range(&self, i: usize) -> (u64, u64) {
        let at = head_len(self.count) + i * BLOCK;
        (at as u64, (self.block_entries(i) * ENTRY) as u64)
    }
}

/// The entries of block `i` of a shard, checked against its digest in the head: sorted, each
/// inside its segment, the block's first key the head's.
pub fn decode_block(object: &str, head: &Head, i: usize, bytes: &[u8]) -> Result<Vec<Entry>, Refusal> {
    let refuse = |why| Refusal::new(object, None, why);
    let want = head.block_entries(i) * ENTRY;
    if bytes.len() != want {
        return Err(refuse(Why::Length { len: bytes.len() as u64, wanted: want as u64 }));
    }
    let (first, digest) = head.blocks[i];
    crate::trust::note(crate::trust::Check::Digest, digest.0);
    if ContentId::of(BLOCK_DOMAIN, bytes) != digest {
        return Err(refuse(Why::Digest));
    }
    let mut out = Vec::with_capacity(head.block_entries(i));
    for chunk in bytes.chunks(ENTRY) {
        let entry = Entry::decode(chunk).map_err(refuse)?;
        if out.last().is_some_and(|it: &Entry| it.key >= entry.key) {
            return Err(refuse(Why::Malformed("a shard block that is not sorted")));
        }
        out.push(entry);
    }
    if out.first().map(|it| it.key) != Some(first) {
        return Err(refuse(Why::Malformed("a shard block whose first key is not its head's")));
    }
    Ok(out)
}

/// Every entry of a whole shard object, checked; a head or block already checked by this
/// client (`known`) is taken as it was and not checked again.
pub fn decode_whole(object: &str, bytes: &[u8], shard: &ShardRef, namespace: Namespace, known: &crate::find::Cache) -> Result<Vec<Entry>, Refusal> {
    let len = shard_len(shard.count);
    if bytes.len() != len {
        return Err(Refusal::new(object, None, Why::Length { len: bytes.len() as u64, wanted: len as u64 }));
    }
    let hl = head_len(shard.count);
    let head = match known.head(&shard.digest) {
        Some(head) => head.clone(),
        None => Head::decode(object, &bytes[..hl], shard, namespace)?,
    };
    let mut out = Vec::with_capacity(shard.count as usize);
    for i in 0..head.blocks.len() {
        if let Some(entries) = known.block(&shard.digest, i) {
            out.extend_from_slice(entries);
            continue;
        }
        let (at, n) = head.block_range(i);
        out.extend(decode_block(object, &head, i, &bytes[at as usize..(at + n) as usize])?);
    }
    if out.first().map(|it| it.key) != Some(shard.first) {
        return Err(Refusal::new(object, None, Why::Malformed("a shard whose first key is not its root's")));
    }
    Ok(out)
}

/// A root's reference to a shard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShardRef {
    pub first: [u8; 32],
    pub digest: ContentId,
    pub count: u32,
}

/// One run: shards disjoint and sorted by first key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Run {
    pub generation: u64,
    pub shards: Vec<ShardRef>,
}

impl Run {
    pub fn entries(&self) -> u64 {
        self.shards.iter().map(|it| u64::from(it.count)).sum()
    }

    /// The shard that would hold `key`, if any.
    pub fn shard_of(&self, key: &[u8; 32]) -> Option<usize> {
        self.shards.partition_point(|it| it.first <= *key).checked_sub(1)
    }

    /// The keys shard `i` covers: from its first key to the next shard's (exclusive), or to
    /// the end.
    pub fn covers(&self, i: usize) -> ([u8; 32], Option<[u8; 32]>) {
        (self.shards[i].first, self.shards.get(i + 1).map(|it| it.first))
    }
}

/// A namespace's root for one toolchain.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Root {
    pub namespace: Namespace,
    pub generation: u64,
    pub keys_serial: u64,
    pub issued: u64,
    pub previous: ContentId,
    /// Newest first.
    pub runs: Vec<Run>,
}

impl Root {
    pub fn encode(&self, signer: &Signer) -> Vec<u8> {
        let mut out = Vec::with_capacity(80 + self.runs.iter().map(|it| 16 + it.shards.len() * 72).sum::<usize>() + 96);
        out.extend_from_slice(b"BLRR");
        put_u32(&mut out, 1);
        put_u64(&mut out, self.namespace.0);
        put_u64(&mut out, self.generation);
        put_u64(&mut out, self.keys_serial);
        put_u64(&mut out, self.issued);
        put_u32(&mut out, self.runs.len() as u32);
        put_u32(&mut out, 0);
        out.extend_from_slice(&self.previous.0);
        for run in &self.runs {
            put_u64(&mut out, run.generation);
            put_u32(&mut out, run.shards.len() as u32);
            put_u32(&mut out, 0);
            for shard in &run.shards {
                out.extend_from_slice(&shard.first);
                out.extend_from_slice(&shard.digest.0);
                put_u32(&mut out, shard.count);
                put_u32(&mut out, 0);
            }
        }
        out.extend_from_slice(&signer.public().0);
        let signature = signer.sign(ROOT_DOMAIN, &out);
        out.extend_from_slice(&signature);
        out
    }

    /// The root in `bytes` and the key that signed it, its signature checked under that key
    /// (whether the key is trusted is the caller's, from the key list the root names).
    pub fn decode(object: &str, bytes: &[u8], namespace: Namespace) -> Result<(Root, PublicKey), Refusal> {
        let refuse = |why| Refusal::new(object, None, why);
        if bytes.len() as u64 > ROOT_MOST {
            return Err(refuse(Why::TooLarge { len: bytes.len() as u64, most: ROOT_MOST }));
        }
        if bytes.len() < 80 + 96 || bytes[0..4] != *b"BLRR" || u32_at(bytes, 4) != 1 {
            return Err(refuse(Why::Malformed("not a root of format 1")));
        }
        let body = bytes.len() - 64;
        let signer = PublicKey(key_at(bytes, body - 32));
        if !verify_sig(&signer, ROOT_DOMAIN, &bytes[..body], &sig_at(bytes, body)) {
            return Err(Refusal::new(object, Some(signer.to_string()), Why::Signature));
        }
        if u64_at(bytes, 8) != namespace.0 && !crate::planted(crate::Plant::SkipNamespace) {
            return Err(refuse(Why::Namespace { found: u64_at(bytes, 8), wanted: namespace.0 }));
        }
        let runs_n = u32_at(bytes, 40) as usize;
        if runs_n > RUNS_MOST {
            return Err(refuse(Why::Malformed("a root of more runs than a reader takes")));
        }
        let end = body - 32;
        let mut at = 80;
        let mut runs = Vec::with_capacity(runs_n);
        for _ in 0..runs_n {
            if at + 16 > end {
                return Err(refuse(Why::Malformed("a root cut short")));
            }
            let generation = u64_at(bytes, at);
            let n = u32_at(bytes, at + 8) as usize;
            at += 16;
            if n == 0 || n > (end - at) / 72 {
                return Err(refuse(Why::Malformed("a run of no shards, or more than the root holds")));
            }
            let mut shards: Vec<ShardRef> = Vec::with_capacity(n);
            for _ in 0..n {
                let shard = ShardRef { first: key_at(bytes, at), digest: id_at(bytes, at + 32), count: u32_at(bytes, at + 64) };
                if shard.count == 0 || shard.count as usize > SHARD_MOST || u32_at(bytes, at + 68) != 0 {
                    return Err(refuse(Why::Malformed("a shard of no entries or too many")));
                }
                if shards.last().is_some_and(|it| it.first >= shard.first) {
                    return Err(refuse(Why::Malformed("a run whose shards are not sorted")));
                }
                shards.push(shard);
                at += 72;
            }
            runs.push(Run { generation, shards });
        }
        if at != end {
            return Err(refuse(Why::Length { len: bytes.len() as u64, wanted: (at + 96) as u64 }));
        }
        let root = Root { namespace, generation: u64_at(bytes, 16), keys_serial: u64_at(bytes, 24), issued: u64_at(bytes, 32), previous: id_at(bytes, 48), runs };
        Ok((root, signer))
    }
}

/// The digest a root's successor names as its previous.
pub fn root_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(ROOT_DOMAIN, bytes)
}

/// Whether the newest two runs are merged: while the newest is at least a quarter of the next
/// (so a run is merged O(log n) times), or while there are more than [`RUNS_KEPT`].
pub fn merge_next(runs: &[Run]) -> bool {
    runs.len() >= 2 && (runs[0].entries() * 4 >= runs[1].entries() || runs.len() > RUNS_KEPT)
}

/// `entries` (sorted, distinct) cut into shards: each shard's entries.
pub fn cut(entries: &[Entry]) -> impl Iterator<Item = &[Entry]> {
    entries.chunks(SHARD_MOST)
}
