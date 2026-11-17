//! Which of a batch of keys the bucket holds: the root, its key list, the heads of the shards
//! the keys fall in, then the blocks they fall in, each step's requests in parallel and each
//! object checked before it is used. Heads, blocks and key lists are immutable and cached; a
//! key found absent is cached as absent under that root's digest (the negative cache), so
//! asking again under the same root costs only the root.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::collections::{BTreeMap, BTreeSet};

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;

use crate::{
    bucket::{Asked, Failure, Machine, Op, Pending, Reply, Step, Ticket, failed},
    name::{self, Toolchain},
    shard::{self, Entry, Head, Root, ShardRef},
    trust::{PublicKey, Refusal, Roles, Trusted, Why},
};

/// Blocks kept in a [`Cache`] before it lets go of them all.
pub const BLOCKS_CACHED: usize = 4096;

/// What this client has fetched and checked, each checked once (decision 18): shard heads and
/// blocks (immutable, by digest), the last root of each namespace (by the digest of its bytes,
/// so the same root read again is not checked again), and the negative cache. Key lists are
/// [`Trusted`]'s, checked once a process. Letting go of blocks past [`BLOCKS_CACHED`] is the
/// one way a block is checked again: a memory bound, counted like any other check.
#[derive(Default, Debug)]
pub struct Cache {
    heads: BTreeMap<ContentId, Head>,
    blocks: BTreeMap<(ContentId, usize), Vec<Entry>>,
    roots: BTreeMap<u64, RootSeen>,
    absent: BTreeMap<u64, (ContentId, BTreeSet<[u8; 32]>)>,
}

impl Cache {
    pub(crate) fn keep_block(&mut self, shard: ContentId, i: usize, entries: Vec<Entry>) {
        self.blocks.insert((shard, i), entries);
    }

    /// Keeps a block a merge checked, while fewer than [`BLOCKS_CACHED`] are held: a merge
    /// walks whole runs, and past the bound a block is let go (and, if needed again, checked
    /// again, counted), never held without limit.
    pub(crate) fn keep_merged_block(&mut self, shard: ContentId, i: usize, entries: Vec<Entry>) {
        if self.blocks.len() < BLOCKS_CACHED {
            self.blocks.insert((shard, i), entries);
        }
    }

    pub(crate) fn keep_head(&mut self, shard: ContentId, head: Head) {
        self.heads.insert(shard, head);
    }

    /// Every entry of `shard`, when its head and every block of it are held (each checked once,
    /// when it was kept): a merge that needs the shard again takes them without a fetch.
    pub(crate) fn whole(&self, shard: &ShardRef) -> Option<Vec<Entry>> {
        let head = self.heads.get(&shard.digest)?;
        let mut out = Vec::with_capacity(shard.count as usize);
        for i in 0..head.blocks.len() {
            out.extend_from_slice(self.blocks.get(&(shard.digest, i))?);
        }
        Some(out)
    }

    pub(crate) fn head(&self, shard: &ContentId) -> Option<&Head> {
        self.heads.get(shard)
    }

    pub(crate) fn block(&self, shard: &ContentId, i: usize) -> Option<&[Entry]> {
        self.blocks.get(&(*shard, i)).map(Vec::as_slice)
    }
}

/// The root as read and trusted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RootSeen {
    pub root: Root,
    pub etag: String,
    pub digest: ContentId,
    pub signer: PublicKey,
    pub bytes_len: u64,
}

/// What a find answered.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Answer {
    /// None: the namespace has no root for this toolchain yet.
    pub root: Option<RootSeen>,
    /// The entries of the keys found, in key order, from the newest run holding each.
    pub found: Vec<Entry>,
}

#[derive(Debug)]
enum Asking {
    Root,
    Keys(u64),
    Head(ShardRef),
    Blocks { shard: ShardRef, first: usize, n: usize },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Start,
    Root,
    Keys,
    Heads,
    Blocks,
}

/// The find machine.
pub struct Find {
    namespace: Namespace,
    toolchain: Toolchain,
    root_name: String,
    root_object: String,
    keys: Vec<[u8; 32]>,
    pub trusted: Trusted,
    pub cache: Cache,
    pending: Pending<Asking>,
    phase: Phase,
    seen: Option<RootSeen>,
    /// Shard digest to the generation of the run naming it (for its object's name).
    generation_of: BTreeMap<ContentId, u64>,
}

impl Find {
    /// A find of `keys` in `namespace`'s root `root_name` for `trusted.toolchain`.
    pub fn new(namespace: Namespace, root_name: &str, mut keys: Vec<[u8; 32]>, trusted: Trusted, cache: Cache) -> Result<Find, Failure> {
        let toolchain = trusted.toolchain.clone();
        let root_object = name::root(namespace, &toolchain, root_name).map_err(|it| Refusal::new(root_name, None, Why::Name(it.to_string())))?;
        keys.sort_unstable();
        keys.dedup();
        let mut cache = cache;
        // Bounded between finds, never during one: a find answers from every block it fetched.
        if cache.blocks.len() > BLOCKS_CACHED {
            cache.blocks.clear();
        }
        Ok(Find {
            namespace,
            toolchain,
            root_name: root_name.to_owned(),
            root_object,
            keys,
            trusted,
            cache,
            pending: Pending::new(),
            phase: Phase::Start,
            seen: None,
            generation_of: BTreeMap::new(),
        })
    }

    pub fn root_name(&self) -> &str {
        &self.root_name
    }

    fn shard_object(&self, digest: &ContentId) -> String {
        let generation = self.generation_of.get(digest).copied().unwrap_or(0);
        name::shard(self.namespace, &self.toolchain, generation, digest).expect("a namespace and toolchain already named")
    }

    fn on(&mut self, asking: Asking, reply: Reply) -> Result<(), Failure> {
        match asking {
            Asking::Root => match reply {
                Reply::Missing => Ok(()),
                Reply::Got { bytes, etag } => {
                    let digest = shard::root_digest(&bytes);
                    let seen = match self.cache.roots.get(&self.namespace.0) {
                        // The root this client checked last: taken as checked.
                        Some(known) if known.digest == digest => RootSeen { etag, ..known.clone() },
                        _ => {
                            let (root, signer) = Root::decode(&self.root_object, &bytes, self.namespace)?;
                            RootSeen { root, etag, digest, signer, bytes_len: bytes.len() as u64 }
                        }
                    };
                    for run in &seen.root.runs {
                        for shard in run.shards() {
                            self.generation_of.insert(shard.digest, run.generation);
                        }
                    }
                    self.cache.roots.insert(self.namespace.0, seen.clone());
                    self.seen = Some(seen);
                    Ok(())
                }
                other => Err(failed(&self.root_object, other)),
            },
            Asking::Keys(serial) => {
                let object = name::keys(self.namespace, serial).expect("a namespace already named");
                match reply {
                    Reply::Got { bytes, .. } => {
                        self.trusted.take_list(&object, self.namespace, &bytes)?;
                        Ok(())
                    }
                    other => Err(failed(&object, other)),
                }
            }
            Asking::Head(shard) => {
                let object = self.shard_object(&shard.digest);
                match reply {
                    Reply::Got { bytes, .. } => {
                        let head = Head::decode(&object, &bytes, &shard, self.namespace)?;
                        self.cache.heads.insert(shard.digest, head);
                        Ok(())
                    }
                    other => Err(failed(&object, other)),
                }
            }
            Asking::Blocks { shard, first, n } => {
                let object = self.shard_object(&shard.digest);
                let bytes = match reply {
                    Reply::Got { bytes, .. } => bytes,
                    other => return Err(failed(&object, other)),
                };
                let head = self.cache.heads.get(&shard.digest).ok_or_else(|| Failure::Misused(format!("{object}: blocks without a head")))?.clone();
                let mut at = 0usize;
                for i in first..first + n {
                    let len = head.block_entries(i) * shard::ENTRY;
                    let Some(block) = bytes.get(at..at + len) else {
                        return Err(Refusal::new(&object, None, Why::Length { len: bytes.len() as u64, wanted: (at + len) as u64 }).into());
                    };
                    let entries = shard::decode_block(&object, &head, i, block)?;
                    self.cache.keep_block(shard.digest, i, entries);
                    at += len;
                }
                if at != bytes.len() {
                    return Err(Refusal::new(&object, None, Why::Length { len: bytes.len() as u64, wanted: at as u64 }).into());
                }
                Ok(())
            }
        }
    }

    /// The keys still to look up: those not cached absent under this root.
    fn open_keys(&self, seen: &RootSeen) -> Vec<[u8; 32]> {
        match self.cache.absent.get(&self.namespace.0) {
            Some((digest, absent)) if *digest == seen.digest => self.keys.iter().filter(|k| !absent.contains(*k)).copied().collect(),
            _ => self.keys.clone(),
        }
    }

    /// Moves on once nothing is pending: the next phase's asks, or the answer.
    fn advance(&mut self, now: u64, out: &mut Vec<Asked>) -> Result<Option<Answer>, Failure> {
        loop {
            match self.phase {
                Phase::Start => {
                    self.pending.ask(out, now, Op::Get { name: self.root_object.clone(), range: None, most: shard::ROOT_MOST }, Asking::Root);
                    self.phase = Phase::Root;
                    return Ok(None);
                }
                Phase::Root => {
                    let Some(seen) = &self.seen else { return Ok(Some(Answer::default())) };
                    let serial = seen.root.keys_serial;
                    self.phase = Phase::Keys;
                    // A key list is checked once a process: the one held is used while it is the
                    // one the root names or a newer one (which decides revocations).
                    if self.trusted.list(self.namespace).is_none_or(|it| it.serial < serial) {
                        let name = name::keys(self.namespace, serial).expect("a namespace already named");
                        self.pending.ask(out, now, Op::Get { name, range: None, most: crate::trust::KEY_LIST_MOST }, Asking::Keys(serial));
                        return Ok(None);
                    }
                }
                Phase::Keys => {
                    let seen = self.seen.as_ref().expect("a root");
                    let list = self.trusted.list(self.namespace).ok_or_else(|| Failure::Misused("no key list after reading it".to_owned()))?;
                    if list.serial < seen.root.keys_serial {
                        return Err(Refusal::new(&self.root_object, None, Why::Malformed("a root naming a key list newer than the one its key list object holds")).into());
                    }
                    list.allows(&self.root_object, &seen.signer, Roles::ROOT, seen.root.issued)?;
                    self.phase = Phase::Heads;
                    let mut wanted: BTreeMap<ContentId, ShardRef> = BTreeMap::new();
                    for key in self.open_keys(seen) {
                        for run in &seen.root.runs {
                            if let Some(i) = run.shard_of(&key) {
                                let shard = run.shards()[i];
                                if !self.cache.heads.contains_key(&shard.digest) {
                                    wanted.insert(shard.digest, shard);
                                }
                            }
                        }
                    }
                    for shard in wanted.into_values() {
                        let name = self.shard_object(&shard.digest);
                        self.pending.ask(out, now, Op::Get { name, range: Some((0, shard::head_len(shard.count) as u64)), most: shard::head_len(shard.count) as u64 }, Asking::Head(shard));
                    }
                    if !self.pending.is_empty() {
                        return Ok(None);
                    }
                }
                Phase::Heads => {
                    let seen = self.seen.as_ref().expect("a root");
                    self.phase = Phase::Blocks;
                    let mut wanted: BTreeMap<ContentId, (ShardRef, BTreeSet<usize>)> = BTreeMap::new();
                    for key in self.open_keys(seen) {
                        for run in &seen.root.runs {
                            let Some(i) = run.shard_of(&key) else { continue };
                            let shard = run.shards()[i];
                            let head = &self.cache.heads[&shard.digest];
                            let Some(b) = head.block_of(&key) else { continue };
                            if !self.cache.blocks.contains_key(&(shard.digest, b)) {
                                wanted.entry(shard.digest).or_insert_with(|| (shard, BTreeSet::new())).1.insert(b);
                            }
                        }
                    }
                    let mut asks = Vec::new();
                    for (digest, (shard, blocks)) in wanted {
                        let head = &self.cache.heads[&digest];
                        let blocks: Vec<usize> = blocks.into_iter().collect();
                        let mut start = 0;
                        while start < blocks.len() {
                            let mut end = start + 1;
                            while end < blocks.len() && blocks[end] == blocks[end - 1] + 1 {
                                end += 1;
                            }
                            let (at, _) = head.block_range(blocks[start]);
                            let (last_at, last_len) = head.block_range(blocks[end - 1]);
                            let len = last_at + last_len - at;
                            asks.push((shard, blocks[start], end - start, at, len));
                            start = end;
                        }
                    }
                    for (shard, first, n, at, len) in asks {
                        let name = self.shard_object(&shard.digest);
                        self.pending.ask(out, now, Op::Get { name, range: Some((at, len)), most: len }, Asking::Blocks { shard, first, n });
                    }
                    if !self.pending.is_empty() {
                        return Ok(None);
                    }
                }
                Phase::Blocks => return Ok(Some(self.answer())),
            }
        }
    }

    fn answer(&mut self) -> Answer {
        let seen = self.seen.clone().expect("a root");
        let open = self.open_keys(&seen);
        let mut found = Vec::new();
        let mut absent = Vec::new();
        for key in open {
            let mut hit = None;
            for run in &seen.root.runs {
                let Some(i) = run.shard_of(&key) else { continue };
                let shard = run.shards()[i];
                let Some(b) = self.cache.heads.get(&shard.digest).and_then(|head| head.block_of(&key)) else { continue };
                if let Some(entries) = self.cache.blocks.get(&(shard.digest, b))
                    && let Ok(at) = entries.binary_search_by(|it| it.key.cmp(&key))
                {
                    hit = Some(entries[at]);
                    break;
                }
            }
            match hit {
                Some(entry) => found.push(entry),
                None => absent.push(key),
            }
        }
        let cached = self.cache.absent.entry(self.namespace.0).or_insert_with(|| (seen.digest, BTreeSet::new()));
        if cached.0 != seen.digest {
            *cached = (seen.digest, BTreeSet::new());
        }
        cached.1.extend(absent);
        Answer { root: Some(seen), found }
    }
}

impl Machine for Find {
    type Out = Answer;
    type Item = ();

    fn step(&mut self, now_ms: u64, replies: Vec<(Ticket, Reply)>) -> Step<Answer> {
        for (ticket, reply) in replies {
            let asking = match self.pending.take(ticket) {
                Ok(it) => it,
                Err(failure) => return Step::Done(Err(failure)),
            };
            if let Err(failure) = self.on(asking, reply) {
                return Step::Done(Err(failure));
            }
        }
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            return Step::Ask(out);
        }
        match self.advance(now_ms, &mut out) {
            Ok(Some(answer)) => Step::Done(Ok(answer)),
            Ok(None) => Step::Ask(out),
            Err(failure) => Step::Done(Err(failure)),
        }
    }
}
