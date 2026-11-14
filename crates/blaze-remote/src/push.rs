//! Pushing: a sans-IO machine over get, head, put and compare-and-swap.
//!
//! In order:
//! 1. Checked before anything is sent: every segment is whole and of the namespace pushed to
//!    (a segment of another namespace is [`Why::Private`]). The fast tier never goes to the
//!    public namespace. The key list verifies under the namespace's anchors and lists the
//!    signer for builds and roots. A second build's (key, value digest) pairs (`rebuilt`)
//!    equal the pushed ones, else [`Why::NotDeterministic`] naming the key.
//! 2. The determinism check against the bucket: a sample of the pushed keys is found there, and
//!    a different value digest is [`Why::NotDeterministic`] naming the key. Merges compare every
//!    key that two runs share.
//! 3. The key list, then every segment the bucket lacks (by a HEAD of its digest's name; one
//!    put, or a multipart upload past [`MULTIPART_OVER`]), each followed by its statement. A
//!    segment already there is not sent again (idempotent by digest; resumable at segment
//!    grain).
//! 4. The index shards of this push's run, and the bundles.
//! 5. Merges, while [`shard::merge_next`] says so: shard by shard of the older run, holding at
//!    most a shard and the newer shards over its range.
//! 6. The root, by compare-and-swap. A root moved by another pusher is read again, re-verified,
//!    and steps 5-6 are redone on it, at most [`ROOT_TRIES`] times, then
//!    [`Failure::Contended`].
//!
//! Each object is put only after everything it names. So a crash at any point leaves the
//! bucket readable at its last root, and the next push sees what is already there by HEAD and
//! completes it.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::collections::{BTreeMap, VecDeque};

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;

use crate::{
    bucket::{Asked, Cond, Failure, Machine, Op, Pending, Reply, Step, Ticket, failed},
    bundle::{Bundle, BundleId, SegmentRef},
    find::{Answer, Cache, Find, RootSeen},
    name::{self, Toolchain},
    segment,
    shard::{self, Entry, Root, Run, SHARD_MOST, ShardRef},
    trust::{KeyList, Refusal, Roles, Signer, Statement, Tier, Trusted, Why},
};

/// Attempts at the root before [`Failure::Contended`].
pub const ROOT_TRIES: u32 = 8;
/// A segment past this goes up in parts.
pub const MULTIPART_OVER: u64 = 64 << 20;
/// A part's size (S3's least is 5 MiB).
pub const PART: usize = 16 << 20;
/// Requests in flight at once.
pub const IN_FLIGHT: usize = 16;

/// A bundle to write: its coordinates and the digests of its segments, all in this push.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BundleOut {
    pub id: BundleId,
    pub segments: Vec<ContentId>,
}

/// Everything a push needs.
pub struct Plan<'a> {
    pub namespace: Namespace,
    pub root_name: String,
    pub tier: Tier,
    pub signer: &'a Signer,
    /// The key list to publish, signed by an anchor: it must list `signer`.
    pub key_list: Vec<u8>,
    /// The namespace's anchors and the toolchain pushed for.
    pub trusted: Trusted,
    pub segments: Vec<&'a [u8]>,
    pub bundles: Vec<BundleOut>,
    /// (key, value digest) of a second, independent build: compared before anything is sent.
    pub rebuilt: Vec<([u8; 32], ContentId)>,
    /// Pushed keys checked against the bucket, per 1024.
    pub sample_per_1024: u32,
    /// Whether the bucket has compare-and-swap on puts.
    pub cas: bool,
    pub now_s: u64,
}

/// What a push did.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Report {
    pub records: u64,
    pub segments_put: u64,
    pub segments_present: u64,
    pub bytes_put: u64,
    pub shards_put: u64,
    pub merges: u64,
    pub generation: u64,
    /// Attempts at the root.
    pub tries: u32,
    pub sampled: u64,
}

struct Seg<'a> {
    bytes: &'a [u8],
    at: SegmentRef,
    records: u32,
}

#[derive(Debug)]
enum Doing {
    KeyList,
    HeadSeg(usize),
    PutSeg(usize),
    Begin(usize),
    Part(usize, u32),
    Finish(usize),
    PutStatement(usize),
    PutShard,
    PutBundle,
    Load(ShardRef, bool),
    PutRoot,
    ReadBack,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Start,
    Finding,
    Upload,
    Shards,
    Merge,
    Root,
    ReadBack,
    Over,
}

struct Merging {
    newer: Run,
    older: Run,
    i: usize,
    /// Shards loaded, by digest: their entries.
    loaded: BTreeMap<ContentId, Vec<Entry>>,
    carry: Vec<Entry>,
    out: Vec<ShardRef>,
}

/// The push machine.
pub struct Push<'a> {
    namespace: Namespace,
    toolchain: Toolchain,
    root_name: String,
    root_object: String,
    signer: &'a Signer,
    tier: Tier,
    key_list: Vec<u8>,
    key_serial: u64,
    segs: Vec<Seg<'a>>,
    entries: Vec<Entry>,
    sample: Vec<[u8; 32]>,
    bundles: Vec<Bundle>,
    cas: bool,
    now_s: u64,
    trusted: Option<Trusted>,
    cache: Option<Cache>,
    find: Option<Box<Find>>,
    rereading: bool,
    phase: Phase,
    pending: Pending<Doing>,
    queue: VecDeque<Doing>,
    uploads: BTreeMap<usize, (String, Vec<Option<String>>)>,
    seen: Option<RootSeen>,
    generation: u64,
    new_run: Option<Run>,
    runs: Vec<Run>,
    merging: Option<Merging>,
    root_digest: Option<ContentId>,
    report: Report,
}

fn refused(object: &str, key: Option<String>, why: Why) -> Failure {
    Failure::Refused(Refusal::new(object, key, why))
}

fn key_hex(key: &[u8; 32]) -> String {
    blaze_format::hex::encode(key)
}

impl<'a> Push<'a> {
    /// The push of `plan`, after the checks of step 1.
    pub fn new(plan: Plan<'a>) -> Result<Push<'a>, Failure> {
        let Plan { namespace, root_name, tier, signer, key_list, mut trusted, segments, bundles, rebuilt, sample_per_1024, cas, now_s } = plan;
        let toolchain = trusted.toolchain.clone();
        let bad = |it: name::BadName| refused(&it.name, None, Why::Name(it.to_string()));
        let root_object = name::root(namespace, &toolchain, &root_name).map_err(bad)?;
        if tier == Tier::Fast && namespace == Namespace::PUBLIC {
            return Err(refused(&root_object, None, Why::Tier));
        }
        let list: KeyList = trusted.take_list("the key list to push", namespace, &key_list)?.clone();
        let keys_object = name::keys(namespace, list.serial).map_err(bad)?;
        list.allows(&keys_object, &signer.public(), Roles::BUILD, now_s)?;
        list.allows(&keys_object, &signer.public(), Roles::ROOT, now_s)?;

        let mut segs = Vec::with_capacity(segments.len());
        let mut entries = Vec::new();
        for (i, bytes) in segments.into_iter().enumerate() {
            let object = format!("segment {i} of this push");
            let read = segment::read(bytes).map_err(|why| refused(&object, None, Why::Malformed(why)))?;
            if read.namespace != namespace && !crate::planted(crate::Plant::SkipPrivacy) {
                let object = name::segment(namespace, &toolchain, &read.digest).map_err(bad)?;
                return Err(refused(&object, None, Why::Private { segment: read.namespace.0, wanted: namespace.0 }));
            }
            let at = SegmentRef { digest: read.digest, len: read.len, index_at: read.index_at, count: read.records.len() as u32 };
            for rec in &read.records {
                entries.push(Entry { key: rec.key, value: rec.value, segment: read.digest, segment_len: read.len, index_at: read.index_at, at: rec.at, len: rec.len, count: at.count });
            }
            segs.push(Seg { bytes, at, records: at.count });
        }
        entries.sort_by(|a, b| a.key.cmp(&b.key).then(a.segment.cmp(&b.segment)));
        let mut kept: Vec<Entry> = Vec::with_capacity(entries.len());
        for entry in entries {
            match kept.last() {
                Some(last) if last.key == entry.key => {
                    if last.value != entry.value {
                        return Err(refused("this push", Some(key_hex(&entry.key)), Why::NotDeterministic { ours: last.value, theirs: entry.value }));
                    }
                }
                _ => kept.push(entry),
            }
        }
        let entries = kept;
        for (key, value) in &rebuilt {
            if let Ok(at) = entries.binary_search_by(|it| it.key.cmp(key))
                && entries[at].value != *value
            {
                return Err(refused("this push against its rebuild", Some(key_hex(key)), Why::NotDeterministic { ours: entries[at].value, theirs: *value }));
            }
        }
        let sample: Vec<[u8; 32]> = entries.iter().map(|it| it.key).filter(|key| u32::from(u16::from_le_bytes([key[0], key[1]])) % 1024 < sample_per_1024).collect();

        let mut outs = Vec::with_capacity(bundles.len());
        for out in bundles {
            let object = out.id.object(namespace).map_err(bad)?;
            if out.id.toolchain != toolchain {
                return Err(refused(&object, None, Why::Toolchain));
            }
            let mut refs = Vec::with_capacity(out.segments.len());
            for digest in &out.segments {
                let Some(seg) = segs.iter().find(|it| it.at.digest == *digest) else {
                    return Err(refused(&object, Some(blaze_format::hex::encode(&digest.0)), Why::Malformed("a bundle names a segment this push does not hold")));
                };
                refs.push(seg.at);
            }
            if refs.len() > crate::bundle::SEGMENTS_MOST {
                return Err(refused(&object, None, Why::TooLarge { len: refs.len() as u64, most: crate::bundle::SEGMENTS_MOST as u64 }));
            }
            outs.push(Bundle { namespace, id: out.id, issued: now_s, keys_serial: list.serial, segments: refs });
        }

        let report = Report { records: entries.len() as u64, sampled: sample.len() as u64, ..Report::default() };
        Ok(Push {
            namespace,
            toolchain,
            root_name,
            root_object,
            signer,
            tier,
            key_list,
            key_serial: list.serial,
            segs,
            entries,
            sample,
            bundles: outs,
            cas,
            now_s,
            trusted: Some(trusted),
            cache: Some(Cache::default()),
            find: None,
            rereading: false,
            phase: Phase::Start,
            pending: Pending::new(),
            queue: VecDeque::new(),
            uploads: BTreeMap::new(),
            seen: None,
            generation: 0,
            new_run: None,
            runs: Vec::new(),
            merging: None,
            root_digest: None,
            report,
        })
    }

    pub fn report(&self) -> Report {
        self.report
    }

    fn seg_object(&self, i: usize) -> String {
        name::segment(self.namespace, &self.toolchain, &self.segs[i].at.digest).expect("named in new")
    }

    fn statement_object(&self, i: usize) -> String {
        name::statement(self.namespace, &self.toolchain, &self.segs[i].at.digest).expect("named in new")
    }

    fn shard_object(&self, generation: u64, digest: &ContentId) -> String {
        name::shard(self.namespace, &self.toolchain, generation, digest).expect("named in new")
    }

    fn parts_of(&self, i: usize) -> u32 {
        self.segs[i].bytes.len().div_ceil(PART) as u32
    }

    /// The op for a queued `doing`, its bytes copied only now.
    fn op_for(&self, doing: &Doing) -> Op {
        match *doing {
            Doing::KeyList => Op::Put { name: name::keys(self.namespace, self.key_serial).expect("named in new"), bytes: self.key_list.clone(), cond: Cond::Absent },
            Doing::HeadSeg(i) => Op::Head { name: self.seg_object(i) },
            Doing::PutSeg(i) => Op::Put { name: self.seg_object(i), bytes: self.segs[i].bytes.to_vec(), cond: Cond::Absent },
            Doing::Begin(i) => Op::Begin { name: self.seg_object(i) },
            Doing::Part(i, n) => {
                let bytes = self.segs[i].bytes;
                let at = (n as usize - 1) * PART;
                let upload = self.uploads[&i].0.clone();
                Op::Part { name: self.seg_object(i), upload, number: n, bytes: bytes[at..(at + PART).min(bytes.len())].to_vec() }
            }
            Doing::Finish(i) => {
                let (upload, etags) = &self.uploads[&i];
                let parts = etags.iter().enumerate().map(|(n, etag)| (n as u32 + 1, etag.clone().expect("every part done before finish"))).collect();
                Op::Finish { name: self.seg_object(i), upload: upload.clone(), parts, cond: Cond::Absent }
            }
            Doing::PutStatement(i) => {
                let seg = &self.segs[i];
                let statement = Statement { namespace: self.namespace, segment: seg.at.digest, len: seg.at.len, toolchain: self.toolchain.digest(), records: seg.records, tier: self.tier, issued: self.now_s, builder: self.signer.public() };
                Op::Put { name: self.statement_object(i), bytes: statement.encode(self.signer), cond: Cond::Absent }
            }
            Doing::PutShard | Doing::PutBundle | Doing::Load(..) | Doing::PutRoot | Doing::ReadBack => unreachable!("asked directly, never queued"),
        }
    }

    fn flush(&mut self, now: u64, out: &mut Vec<Asked>) {
        while self.pending.len() < IN_FLIGHT {
            let Some(doing) = self.queue.pop_front() else { break };
            let op = self.op_for(&doing);
            self.report.bytes_put += op.sends();
            self.pending.ask(out, now, op, doing);
        }
    }

    fn ask(&mut self, now: u64, out: &mut Vec<Asked>, op: Op, doing: Doing) {
        self.report.bytes_put += op.sends();
        self.pending.ask(out, now, op, doing);
    }

    fn on(&mut self, doing: Doing, reply: Reply) -> Result<(), Failure> {
        match doing {
            Doing::KeyList => match reply {
                Reply::Done { .. } | Reply::Precondition => Ok(()),
                other => Err(failed(&name::keys(self.namespace, self.key_serial).expect("named"), other)),
            },
            Doing::HeadSeg(i) => match reply {
                Reply::Missing => {
                    self.queue.push_back(if self.segs[i].at.len > MULTIPART_OVER { Doing::Begin(i) } else { Doing::PutSeg(i) });
                    Ok(())
                }
                Reply::Exists { len } if len == self.segs[i].at.len => {
                    self.report.segments_present += 1;
                    self.queue.push_back(Doing::PutStatement(i));
                    Ok(())
                }
                Reply::Exists { len } => Err(refused(&self.seg_object(i), None, Why::Length { len, wanted: self.segs[i].at.len })),
                other => Err(failed(&self.seg_object(i), other)),
            },
            Doing::PutSeg(i) | Doing::Finish(i) => match reply {
                // Another pusher put the same digest: the same bytes.
                Reply::Done { .. } | Reply::Precondition => {
                    self.report.segments_put += 1;
                    self.queue.push_back(Doing::PutStatement(i));
                    Ok(())
                }
                other => Err(failed(&self.seg_object(i), other)),
            },
            Doing::Begin(i) => match reply {
                Reply::Begun { upload } => {
                    let parts = self.parts_of(i);
                    self.uploads.insert(i, (upload, vec![None; parts as usize]));
                    for n in 1..=parts {
                        self.queue.push_back(Doing::Part(i, n));
                    }
                    Ok(())
                }
                other => Err(failed(&self.seg_object(i), other)),
            },
            Doing::Part(i, n) => match reply {
                Reply::Done { etag } => {
                    let (_, etags) = self.uploads.get_mut(&i).expect("begun");
                    etags[n as usize - 1] = Some(etag);
                    if etags.iter().all(Option::is_some) {
                        self.queue.push_back(Doing::Finish(i));
                    }
                    Ok(())
                }
                other => Err(failed(&self.seg_object(i), other)),
            },
            Doing::PutStatement(i) => match reply {
                // A statement already there is another valid one for the same bytes.
                Reply::Done { .. } | Reply::Precondition => Ok(()),
                other => Err(failed(&self.statement_object(i), other)),
            },
            Doing::PutShard => match reply {
                Reply::Done { .. } | Reply::Precondition => {
                    self.report.shards_put += 1;
                    Ok(())
                }
                other => Err(failed("an index shard", other)),
            },
            Doing::PutBundle => match reply {
                Reply::Done { .. } => Ok(()),
                other => Err(failed("a bundle manifest", other)),
            },
            Doing::Load(shard, older) => {
                let merging = self.merging.as_mut().ok_or_else(|| Failure::Misused("a shard loaded with no merge".to_owned()))?;
                let generation = if older { merging.older.generation } else { merging.newer.generation };
                let object = name::shard(self.namespace, &self.toolchain, generation, &shard.digest).expect("named");
                match reply {
                    Reply::Got { bytes, .. } => {
                        let empty = Cache::default();
                        let known = self.cache.as_ref().unwrap_or(&empty);
                        let entries = shard::decode_whole(&object, &bytes, &shard, self.namespace, known)?;
                        merging.loaded.insert(shard.digest, entries);
                        Ok(())
                    }
                    other => Err(failed(&object, other)),
                }
            }
            Doing::PutRoot => match reply {
                Reply::Done { .. } => {
                    self.phase = if self.cas { Phase::Over } else { Phase::ReadBack };
                    Ok(())
                }
                Reply::Precondition => {
                    if self.report.tries >= ROOT_TRIES {
                        return Err(Failure::Contended { root: self.root_object.clone(), tries: self.report.tries });
                    }
                    self.start_find(true)
                }
                other => Err(failed(&self.root_object, other)),
            },
            Doing::ReadBack => match reply {
                Reply::Got { bytes, .. } if Some(shard::root_digest(&bytes)) == self.root_digest => {
                    self.phase = Phase::Over;
                    Ok(())
                }
                Reply::Got { .. } | Reply::Missing => Err(Failure::LostRace { root: self.root_object.clone() }),
                other => Err(failed(&self.root_object, other)),
            },
        }
    }

    fn start_find(&mut self, reread: bool) -> Result<(), Failure> {
        let keys = if reread { Vec::new() } else { std::mem::take(&mut self.sample) };
        let trusted = self.trusted.take().ok_or_else(|| Failure::Misused("two finds at once".to_owned()))?;
        let cache = self.cache.take().unwrap_or_default();
        self.find = Some(Box::new(Find::new(self.namespace, &self.root_name, keys, trusted, cache)?));
        self.rereading = reread;
        self.phase = Phase::Finding;
        Ok(())
    }

    fn after_find(&mut self, answer: Answer) -> Result<(), Failure> {
        if let Some(seen) = &answer.root
            && seen.root.keys_serial > self.key_serial
        {
            return Err(refused(&name::keys(self.namespace, self.key_serial).expect("named"), None, Why::StaleKeyList { serial: self.key_serial, seen: seen.root.keys_serial }));
        }
        if !self.rereading {
            for theirs in &answer.found {
                if let Ok(at) = self.entries.binary_search_by(|it| it.key.cmp(&theirs.key))
                    && self.entries[at].value != theirs.value
                {
                    return Err(refused(&self.root_object, Some(key_hex(&theirs.key)), Why::NotDeterministic { ours: self.entries[at].value, theirs: theirs.value }));
                }
            }
        }
        self.seen = answer.root;
        self.generation = self.seen.as_ref().map_or(0, |it| it.root.generation) + 1;
        if self.rereading {
            self.runs = self.new_run.iter().cloned().chain(self.seen.iter().flat_map(|it| it.root.runs.iter().cloned())).collect();
            self.merging = None;
            self.phase = Phase::Merge;
        } else {
            self.phase = Phase::Upload;
            self.queue.push_back(Doing::KeyList);
            for i in 0..self.segs.len() {
                self.queue.push_back(Doing::HeadSeg(i));
            }
        }
        Ok(())
    }

    fn put_shard(&mut self, now: u64, out: &mut Vec<Asked>, generation: u64, entries: &[Entry]) -> ShardRef {
        let (digest, bytes) = shard::encode(self.namespace, generation, entries);
        let name = self.shard_object(generation, &digest);
        self.ask(now, out, Op::Put { name, bytes, cond: Cond::Absent }, Doing::PutShard);
        ShardRef { first: entries[0].key, digest, count: entries.len() as u32 }
    }

    fn put_root(&mut self, now: u64, out: &mut Vec<Asked>) {
        self.report.tries += 1;
        self.report.generation = self.generation;
        let root = Root {
            namespace: self.namespace,
            generation: self.generation,
            keys_serial: self.key_serial,
            issued: self.now_s,
            previous: self.seen.as_ref().map_or(ContentId([0; 32]), |it| it.digest),
            runs: self.runs.clone(),
        };
        let bytes = root.encode(self.signer);
        self.root_digest = Some(shard::root_digest(&bytes));
        let cond = match (&self.seen, self.cas && !crate::planted(crate::Plant::IgnoreCas)) {
            (_, false) => Cond::Any,
            (Some(seen), true) => Cond::Matches(seen.etag.clone()),
            (None, true) => Cond::Absent,
        };
        self.ask(now, out, Op::Put { name: self.root_object.clone(), bytes, cond }, Doing::PutRoot);
        self.phase = Phase::Root;
    }

    /// One merge step with nothing pending: the loads the next chunk needs, or the chunk
    /// merged; the run finished when the older run is done.
    fn merge_step(&mut self, now: u64, out: &mut Vec<Asked>) -> Result<(), Failure> {
        let generation = self.generation;
        let m = self.merging.as_mut().expect("merging");
        if m.i == m.older.shards.len() {
            let carry = std::mem::take(&mut m.carry);
            let mut shards = std::mem::take(&mut m.out);
            if !carry.is_empty() {
                shards.push(self.put_shard(now, out, generation, &carry));
            }
            let merged = Run { generation, shards };
            self.runs.splice(0..2, [merged]);
            self.merging = None;
            self.report.merges += 1;
            return Ok(());
        }
        let (lo, hi) = (if m.i == 0 { None } else { Some(m.older.shards[m.i].first) }, m.older.covers(m.i).1);
        let mut wanted: Vec<(ShardRef, bool)> = Vec::new();
        let older = m.older.shards[m.i];
        if !m.loaded.contains_key(&older.digest) {
            wanted.push((older, true));
        }
        let overlapping: Vec<usize> = (0..m.newer.shards.len())
            .filter(|&j| {
                let (first, next) = m.newer.covers(j);
                hi.is_none_or(|hi| first < hi) && match (next, lo) {
                    (Some(next), Some(lo)) => next > lo,
                    _ => true,
                }
            })
            .collect();
        for &j in &overlapping {
            let shard = m.newer.shards[j];
            if !m.loaded.contains_key(&shard.digest) {
                wanted.push((shard, false));
            }
        }
        if !wanted.is_empty() {
            let (older_gen, newer_gen) = (m.older.generation, m.newer.generation);
            for (shard, is_older) in wanted {
                let name = self.shard_object(if is_older { older_gen } else { newer_gen }, &shard.digest);
                self.ask(now, out, Op::Get { name, range: None, most: shard::shard_len(shard.count) as u64 }, Doing::Load(shard, is_older));
            }
            return Ok(());
        }
        let older_entries = m.loaded.remove(&older.digest).expect("loaded");
        let in_range = |key: &[u8; 32]| lo.is_none_or(|lo| *key >= lo) && hi.is_none_or(|hi| *key < hi);
        let mut newer_entries: Vec<Entry> = Vec::new();
        for &j in &overlapping {
            newer_entries.extend(m.loaded[&m.newer.shards[j].digest].iter().filter(|it| in_range(&it.key)).copied());
        }
        // Newer shards whose range ends inside this chunk are not needed again.
        for &j in &overlapping {
            if let (Some(next), Some(hi)) = (m.newer.covers(j).1, hi)
                && next <= hi
            {
                m.loaded.remove(&m.newer.shards[j].digest);
            }
        }
        let (mut a, mut b) = (0, 0);
        while a < older_entries.len() || b < newer_entries.len() {
            let take_newer = match (older_entries.get(a), newer_entries.get(b)) {
                (Some(o), Some(n)) if o.key == n.key => {
                    if o.value != n.value {
                        return Err(refused(&self.root_object, Some(key_hex(&o.key)), Why::NotDeterministic { ours: n.value, theirs: o.value }));
                    }
                    a += 1;
                    true
                }
                (Some(o), Some(n)) => n.key < o.key,
                (None, Some(_)) => true,
                (Some(_), None) => false,
                (None, None) => unreachable!("the loop's condition"),
            };
            if take_newer {
                m.carry.push(newer_entries[b]);
                b += 1;
            } else {
                m.carry.push(older_entries[a]);
                a += 1;
            }
        }
        m.i += 1;
        while m.carry.len() >= SHARD_MOST {
            let full: Vec<Entry> = m.carry.drain(..SHARD_MOST).collect();
            let shard = self.put_shard(now, out, generation, &full);
            self.merging.as_mut().expect("merging").out.push(shard);
            return Ok(());
        }
        Ok(())
    }

    /// Moves on once nothing is pending or queued.
    fn advance(&mut self, now: u64, out: &mut Vec<Asked>) -> Result<bool, Failure> {
        loop {
            self.flush(now, out);
            if !self.pending.is_empty() {
                return Ok(false);
            }
            match self.phase {
                Phase::Start => {
                    self.start_find(false)?;
                    return Ok(false);
                }
                Phase::Finding => return Ok(false),
                Phase::Upload => {
                    self.phase = Phase::Shards;
                    if !self.entries.is_empty() {
                        let entries = std::mem::take(&mut self.entries);
                        let mut shards = Vec::new();
                        for part in shard::cut(&entries) {
                            shards.push(self.put_shard(now, out, self.generation, part));
                        }
                        self.entries = entries;
                        self.new_run = Some(Run { generation: self.generation, shards });
                    }
                    for i in 0..self.bundles.len() {
                        let bundle = &self.bundles[i];
                        let name = bundle.id.object(self.namespace).expect("named in new");
                        let bytes = bundle.encode(self.signer);
                        self.ask(now, out, Op::Put { name, bytes, cond: Cond::Any }, Doing::PutBundle);
                    }
                    if crate::planted(crate::Plant::RootBeforeShards) {
                        self.phase = Phase::Merge;
                        self.runs = self.new_run.iter().cloned().chain(self.seen.iter().flat_map(|it| it.root.runs.iter().cloned())).collect();
                        self.put_root(now, out);
                        return Ok(false);
                    }
                }
                Phase::Shards => {
                    if self.new_run.is_none() {
                        // Nothing to index: the bundles are written, the root stays.
                        return Ok(true);
                    }
                    self.runs = self.new_run.iter().cloned().chain(self.seen.iter().flat_map(|it| it.root.runs.iter().cloned())).collect();
                    self.phase = Phase::Merge;
                }
                Phase::Merge => {
                    if self.merging.is_some() {
                        self.merge_step(now, out)?;
                        continue;
                    }
                    if shard::merge_next(&self.runs) {
                        self.merging = Some(Merging { newer: self.runs[0].clone(), older: self.runs[1].clone(), i: 0, loaded: BTreeMap::new(), carry: Vec::new(), out: Vec::new() });
                        continue;
                    }
                    self.put_root(now, out);
                }
                Phase::Root => return Err(Failure::Misused("the root's answer did not move the push on".to_owned())),
                Phase::ReadBack => {
                    self.ask(now, out, Op::Get { name: self.root_object.clone(), range: None, most: shard::ROOT_MOST }, Doing::ReadBack);
                    self.phase = Phase::Root;
                }
                Phase::Over => return Ok(true),
            }
        }
    }
}

impl Machine for Push<'_> {
    type Out = Report;
    type Item = ();

    fn step(&mut self, now_ms: u64, replies: Vec<(Ticket, Reply)>) -> Step<Report> {
        let mut out = Vec::new();
        let mut replies = replies;
        loop {
            if self.phase == Phase::Finding {
                let find = self.find.as_mut().expect("finding");
                match find.step(now_ms, std::mem::take(&mut replies)) {
                    Step::Ask(asked) => return Step::Ask(asked),
                    Step::Done(Err(failure)) => return Step::Done(Err(failure)),
                    Step::Done(Ok(answer)) => {
                        let find = *self.find.take().expect("finding");
                        let (trusted, cache) = (find.trusted, find.cache);
                        self.trusted = Some(trusted);
                        self.cache = Some(cache);
                        if let Err(failure) = self.after_find(answer) {
                            return Step::Done(Err(failure));
                        }
                    }
                }
            } else {
                for (ticket, reply) in std::mem::take(&mut replies) {
                    let doing = match self.pending.take(ticket) {
                        Ok(it) => it,
                        Err(failure) => return Step::Done(Err(failure)),
                    };
                    if let Err(failure) = self.on(doing, reply) {
                        return Step::Done(Err(failure));
                    }
                }
            }
            if self.phase == Phase::Finding {
                // A find started by a root's answer: its first asks.
                continue;
            }
            match self.advance(now_ms, &mut out) {
                Err(failure) => return Step::Done(Err(failure)),
                Ok(true) => return Step::Done(Ok(self.report)),
                Ok(false) if self.phase == Phase::Finding => {
                    // A find started by `advance`: its first asks join these.
                    let find = self.find.as_mut().expect("finding");
                    match find.step(now_ms, Vec::new()) {
                        Step::Ask(asked) => {
                            out.extend(asked);
                            return Step::Ask(out);
                        }
                        Step::Done(Err(failure)) => return Step::Done(Err(failure)),
                        Step::Done(Ok(_)) => return Step::Done(Err(Failure::Misused("a find that ended before it asked".to_owned()))),
                    }
                }
                Ok(false) => return Step::Ask(out),
            }
        }
    }
}
