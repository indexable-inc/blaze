//! Pulling: segments are fetched whole, each with its statement. Each segment is checked exactly
//! once (decision 18, verify once by construction), at one of two places, never both:
//!
//! - For the plain store, the store checks: [`Remote`] is its [`Source`] and hands it the
//!   unchecked bytes. [`Trusted::verifier`] of the namespace pulled is its
//!   [`Verifier`](blaze_keyed::store::Verifier) (the statement). The store checks the digest and
//!   layout, adopts the segment, and records it in its manifest, so no later process checks it
//!   again.
//! - For any other taker (a test's model of the store, a remote worker), [`Trusted::prove`]
//!   consumes a [`Fetched`] and returns a [`Proven`], which only it can make.
//!
//! A [`Fetched`] gives no access to its bytes except through one of those two. A check checks
//! that the statement is signed by a builder the namespace's key list trusts, and names this
//! namespace, toolchain, digest and length. It also checks that the bytes hash to that digest
//! and are a well-formed plain segment of this namespace.
//!
//! The rule is whole segments, never ranged records. A segment's trust is its digest over every
//! byte, and the plain store adopts and maps segments whole. A ranged record could be checked
//! against its value digest in the index, but the store has no form for a lone record from
//! elsewhere. The cost is the segment's other records, bounded by the pusher's segment size.
//!
//! Call [`Remote::prepare`] before the store's `fetch_missing` or `pull`, so the verifier holds
//! the namespace's key list.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{
    collections::{BTreeMap, VecDeque},
    ops::Range,
};

use blaze_cas::plain::{Namespace, Source};
use blaze_keyed::{
    exec::RecordRef,
    store::{Located, StoreError},
};

use crate::{
    bucket::{Asked, Bucket, Counts, Failure, Machine, Op, Pending, Reply, Step, Ticket, drive, failed},
    bundle::{Bundle, BundleId},
    find::{Answer, Cache, Find},
    name,
    segment::{self, SegmentId, SegmentRef},
    shard::Entry,
    trust::{Refusal, Roles, Statement, Trusted, Why},
};

/// Segments fetched at once (each with its statement: twice as many requests).
pub const IN_FLIGHT: usize = 8;

/// A segment and its statement as fetched: not checked, and so not readable.
#[derive(Debug)]
pub struct Fetched {
    namespace: Namespace,
    segment: SegmentRef,
    bytes: Vec<u8>,
    statement: Vec<u8>,
}

impl Fetched {
    pub fn segment(&self) -> &SegmentRef {
        &self.segment
    }
}

/// A segment checked by [`Trusted::prove`]: the only way to make one.
#[derive(Debug)]
pub struct Proven {
    namespace: Namespace,
    segment: SegmentRef,
    statement: Statement,
    bytes: Vec<u8>,
    /// Its records, as the one check read them: a taker reads these, never the bytes again.
    read: segment::Read,
}

impl Proven {
    /// Its records (key, place, tag, value digest), read by the check.
    pub fn records(&self) -> &[segment::Rec] {
        &self.read.records
    }

    /// The payload of `rec`, one of [`Proven::records`].
    pub fn payload(&self, rec: &segment::Rec) -> &[u8] {
        &self.bytes[rec.at as usize..rec.at as usize + rec.len as usize]
    }

    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    pub fn segment(&self) -> &SegmentRef {
        &self.segment
    }

    pub fn statement(&self) -> &Statement {
        &self.statement
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Trusted {
    /// The one check of a fetched segment: its statement (by the namespace's verifier), then
    /// its bytes, by the store's own segment check.
    pub fn prove(&self, fetched: Fetched) -> Result<Proven, Refusal> {
        let Fetched { namespace, segment: want, bytes, statement } = fetched;
        let object = name::segment(namespace, &self.toolchain, &want.digest().0).map_err(|it| Refusal::new("a segment", None, Why::Name(it.to_string())))?;
        let st_object = format!("{object}.statement");
        let statement = self.statement(&st_object, namespace, &want, &statement)?;
        if bytes.len() as u64 != want.size() {
            return Err(Refusal::new(&object, None, Why::Length { len: bytes.len() as u64, wanted: want.size() }));
        }
        let read = segment::read(&bytes).map_err(|why| Refusal::new(&object, None, Why::Segment(why)))?;
        if read.segment.digest() != want.digest() && !crate::planted(crate::Plant::SkipSegmentDigest) {
            return Err(Refusal::new(&object, None, Why::Digest));
        }
        if read.namespace != namespace && !crate::planted(crate::Plant::SkipNamespace) {
            return Err(Refusal::new(&object, None, Why::Namespace { found: read.namespace.0, wanted: namespace.0 }));
        }
        if read.segment.index_at() != want.index_at() || read.segment.count() != want.count() {
            return Err(Refusal::new(&object, None, Why::Malformed("a segment whose index is not where its reference says")));
        }
        Ok(Proven { namespace, segment: want, statement, bytes, read })
    }
}

#[derive(Debug)]
enum Part {
    Bytes(SegmentId),
    Statement(SegmentId),
}

#[derive(Default, Debug)]
struct Arriving {
    bytes: Option<Vec<u8>>,
    statement: Option<Vec<u8>>,
}

/// Fetches named segments with their statements, at most [`IN_FLIGHT`] at once, and hands each
/// on as [`Fetched`], unchecked.
pub struct Fetch {
    namespace: Namespace,
    toolchain: name::Toolchain,
    wanted: VecDeque<SegmentRef>,
    arriving: BTreeMap<SegmentId, (SegmentRef, Arriving)>,
    pending: Pending<Part>,
    ready: VecDeque<Fetched>,
    handed: usize,
}

impl Fetch {
    pub fn new(namespace: Namespace, toolchain: name::Toolchain, wanted: Vec<SegmentRef>) -> Fetch {
        let mut seen = std::collections::BTreeSet::new();
        let wanted = wanted.into_iter().filter(|it| seen.insert(it.digest())).collect();
        Fetch { namespace, toolchain, wanted, arriving: BTreeMap::new(), pending: Pending::new(), ready: VecDeque::new(), handed: 0 }
    }

    fn names(&self, digest: &SegmentId) -> (String, String) {
        (name::segment(self.namespace, &self.toolchain, &digest.0).expect("a namespace already named"), name::statement(self.namespace, &self.toolchain, &digest.0).expect("a namespace already named"))
    }

    fn on(&mut self, part: Part, reply: Reply) -> Result<(), Failure> {
        let (digest, is_bytes) = match part {
            Part::Bytes(d) => (d, true),
            Part::Statement(d) => (d, false),
        };
        let (object, st_object) = self.names(&digest);
        let got = match reply {
            Reply::Got { bytes, .. } => bytes,
            other => return Err(failed(if is_bytes { &object } else { &st_object }, other)),
        };
        let (_, arriving) = self.arriving.get_mut(&digest).ok_or_else(|| Failure::Misused(format!("{object}: an answer for a segment not asked")))?;
        if is_bytes {
            arriving.bytes = Some(got);
        } else {
            arriving.statement = Some(got);
        }
        if arriving.bytes.is_some() && arriving.statement.is_some() {
            let (want, arriving) = self.arriving.remove(&digest).expect("just seen");
            self.ready.push_back(Fetched { namespace: self.namespace, segment: want, bytes: arriving.bytes.expect("both"), statement: arriving.statement.expect("both") });
        }
        Ok(())
    }

    /// Asks for the next segments. A reference's length is within the most a segment is by its
    /// type ([`SegmentRef::new`]), so no size is checked here.
    fn refill(&mut self, now: u64, out: &mut Vec<Asked>) {
        while self.arriving.len() < IN_FLIGHT {
            let Some(want) = self.wanted.pop_front() else { break };
            let (object, st_object) = self.names(&want.digest());
            self.arriving.insert(want.digest(), (want, Arriving::default()));
            self.pending.ask(out, now, Op::Get { name: st_object, range: None, most: Statement::LEN as u64 }, Part::Statement(want.digest()));
            self.pending.ask(out, now, Op::Get { name: object, range: None, most: want.size() }, Part::Bytes(want.digest()));
        }
    }
}

impl Machine for Fetch {
    type Out = usize;
    type Item = Fetched;

    fn step(&mut self, now_ms: u64, replies: Vec<(Ticket, Reply)>) -> Step<usize> {
        for (ticket, reply) in replies {
            let part = match self.pending.take(ticket) {
                Ok(it) => it,
                Err(failure) => return Step::Done(Err(failure)),
            };
            if let Err(failure) = self.on(part, reply) {
                return Step::Done(Err(failure));
            }
        }
        let mut out = Vec::new();
        self.refill(now_ms, &mut out);
        // What is ready is taken by the driver before it looks at the step, a last step too.
        if self.pending.is_empty() && self.wanted.is_empty() {
            return Step::Done(Ok(self.handed + self.ready.len()));
        }
        Step::Ask(out)
    }

    fn take(&mut self) -> Option<Fetched> {
        let it = self.ready.pop_front()?;
        self.handed += 1;
        Some(it)
    }
}

/// Fetches one object, for the store's [`Source`] calls and a manifest.
struct GetOne {
    op: Option<Op>,
    object: String,
    got: Option<Vec<u8>>,
    pending: Pending<()>,
}

impl Machine for GetOne {
    type Out = Vec<u8>;
    type Item = ();

    fn step(&mut self, now_ms: u64, replies: Vec<(Ticket, Reply)>) -> Step<Vec<u8>> {
        for (ticket, reply) in replies {
            if let Err(failure) = self.pending.take(ticket) {
                return Step::Done(Err(failure));
            }
            match reply {
                Reply::Got { bytes, .. } => self.got = Some(bytes),
                other => return Step::Done(Err(failed(&self.object, other))),
            }
        }
        if let Some(bytes) = self.got.take() {
            return Step::Done(Ok(bytes));
        }
        let mut out = Vec::new();
        if let Some(op) = self.op.take() {
            self.pending.ask(&mut out, now_ms, op, ());
        }
        Step::Ask(out)
    }
}

/// What a pull did, by its counts.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Report {
    pub found: usize,
    pub segments: usize,
    pub counts: Counts,
}

/// The bucket as the plain store's [`Source`].
pub struct Remote<'b> {
    bucket: &'b mut dyn Bucket,
    root_name: String,
    trusted: Option<Trusted>,
    cache: Cache,
    /// The namespace each segment found was found in: [`Source::fetch_into`] names none.
    located: BTreeMap<SegmentId, Namespace>,
    /// Segments fetched ahead (a bundle), unchecked, until the store asks for them.
    held: BTreeMap<SegmentId, Fetched>,
    pub report: Report,
}

impl<'b> Remote<'b> {
    pub fn new(bucket: &'b mut dyn Bucket, root_name: &str, trusted: Trusted) -> Remote<'b> {
        Remote { bucket, root_name: root_name.to_owned(), trusted: Some(trusted), cache: Cache::default(), located: BTreeMap::new(), held: BTreeMap::new(), report: Report::default() }
    }

    /// What is trusted now: its [`Trusted::verifier`] of the namespace pulled is what to hand
    /// the store with this source.
    pub fn trusted(&self) -> &Trusted {
        self.trusted.as_ref().expect("held between calls")
    }

    fn toolchain(&self) -> name::Toolchain {
        self.trusted().toolchain.clone()
    }

    /// Finds `keys` in `namespace`: the entries found, the root read.
    pub fn find_entries(&mut self, namespace: Namespace, keys: Vec<[u8; 32]>) -> Result<Answer, Failure> {
        let trusted = self.trusted.take().expect("held between calls");
        let mut find = Find::new(namespace, &self.root_name, keys, trusted, std::mem::take(&mut self.cache))?;
        let answer = drive(&mut find, &mut *self.bucket, &mut |()| Ok(()));
        self.trusted = Some(find.trusted);
        self.cache = find.cache;
        self.report.counts = self.bucket.counts();
        let answer = answer?;
        for entry in &answer.found {
            self.located.insert(entry.segment().digest(), namespace);
        }
        self.report.found += answer.found.len();
        Ok(answer)
    }

    /// Reads `namespace`'s root and key list, so [`Remote::trusted`] can verify its statements.
    pub fn prepare(&mut self, namespace: Namespace) -> Result<Answer, Failure> {
        self.find_entries(namespace, Vec::new())
    }

    /// Fetches the segments holding `keys` in `namespace` and hands each to `sink` proven.
    pub fn pull(&mut self, namespace: Namespace, keys: Vec<[u8; 32]>, sink: &mut dyn FnMut(Proven) -> Result<(), String>) -> Result<usize, Failure> {
        let answer = self.find_entries(namespace, keys)?;
        let wanted = answer.found.iter().map(|it| *it.segment()).collect();
        let mut fetch = Fetch::new(namespace, self.toolchain(), wanted);
        let trusted = self.trusted.take().expect("held between calls");
        let done = drive(&mut fetch, &mut *self.bucket, &mut |fetched| sink(trusted.prove(fetched).map_err(|it| it.to_string())?));
        self.trusted = Some(trusted);
        self.report.counts = self.bucket.counts();
        // A refusal by `prove` comes through the sink, as its text.
        let done = done?;
        self.report.segments += done;
        Ok(done)
    }

    /// Reads the bundle `id` of `namespace`: its manifest, checked (signed by a builder the key
    /// list it names trusts), and every segment it names fetched ahead in parallel and held
    /// unchecked for the store, which checks each as it pulls it. The segments, as the store's
    /// `pull` takes them.
    pub fn bundle_ahead(&mut self, namespace: Namespace, id: &BundleId) -> Result<(Bundle, Vec<SegmentRef>), Failure> {
        let object = id.object(namespace).map_err(|it| Refusal::new(id.toolchain.as_str(), None, Why::Name(it.to_string())))?;
        if self.trusted().toolchain != id.toolchain {
            return Err(Refusal::new(&object, None, Why::Toolchain).into());
        }
        let bytes = self.get(Op::Get { name: object.clone(), range: None, most: crate::bundle::MANIFEST_MOST }, &object)?;
        let (bundle, signer) = Bundle::decode(&object, &bytes, namespace, id)?;
        let held_serial = self.trusted().list(namespace).map(|it| it.serial);
        if held_serial.is_none_or(|serial| serial < bundle.keys_serial) {
            let keys_object = name::keys(namespace, bundle.keys_serial).map_err(|it| Failure::Misused(it.to_string()))?;
            let keys = self.get(Op::Get { name: keys_object.clone(), range: None, most: crate::trust::KEY_LIST_MOST }, &keys_object)?;
            self.trusted.as_mut().expect("held between calls").take_list(&keys_object, namespace, &keys)?;
        }
        self.trusted().list(namespace).expect("just taken").allows(&object, &signer, Roles::BUILD, bundle.issued)?;
        let mut fetch = Fetch::new(namespace, self.toolchain(), bundle.segments.to_vec());
        let mut held = Vec::new();
        let done = drive(&mut fetch, &mut *self.bucket, &mut |fetched| {
            held.push(fetched);
            Ok(())
        });
        self.report.counts = self.bucket.counts();
        done?;
        for fetched in held {
            self.located.insert(fetched.segment.digest(), namespace);
            self.held.insert(fetched.segment.digest(), fetched);
        }
        self.report.segments += bundle.segments.len();
        let segments = bundle.segments.to_vec();
        Ok((bundle, segments))
    }

    /// [`Remote::bundle_ahead`], each segment proven here and handed to `sink` (a taker that is
    /// not the plain store).
    pub fn bundle(&mut self, namespace: Namespace, id: &BundleId, sink: &mut dyn FnMut(Proven) -> Result<(), String>) -> Result<Bundle, Failure> {
        let (bundle, _) = self.bundle_ahead(namespace, id)?;
        for it in &bundle.segments {
            let fetched = self.held.remove(&it.digest()).ok_or_else(|| Failure::Misused("a bundle's segment not held".to_owned()))?;
            let proven = self.trusted().prove(fetched)?;
            sink(proven).map_err(Failure::Sink)?;
        }
        Ok(bundle)
    }

    fn get(&mut self, op: Op, object: &str) -> Result<Vec<u8>, Failure> {
        let mut one = GetOne { op: Some(op), object: object.to_owned(), got: None, pending: Pending::new() };
        let got = drive(&mut one, &mut *self.bucket, &mut |()| Ok(()));
        self.report.counts = self.bucket.counts();
        got
    }

    fn located(&self, segment: &SegmentId) -> Result<Namespace, StoreError> {
        self.located.get(segment).copied().ok_or(StoreError::Other {
            what: "a segment asked of the remote source before a find or a bundle named it: its namespace is not known, so it is not fetched",
        })
    }
}

/// A name built from parts already validated: one that fails is a bug here, said as one.
fn named(name: Result<String, name::BadName>) -> Result<String, StoreError> {
    name.map_err(|_| StoreError::Other { what: "a bucket object's name from parts already validated" })
}

/// A failure of the bucket as the store's error, at the [`Source`] boundary. The object a
/// failure names is not carried: `StoreError::Remote` holds a fixed phrase and a status (0: the
/// bucket gave none; 404: an object a signed object names is missing), and nothing else here
/// fits (the report lists the variant this wants).
pub fn store_error(failure: &Failure) -> StoreError {
    let remote = |what| StoreError::Remote { what, status: 0 };
    match failure {
        Failure::Refused(refusal) => match &refusal.why {
            Why::Segment(why) => why.clone(),
            Why::Wire(why) => StoreError::Wire { what: "an object of the bucket", why: *why },
            why => remote(refused(why)),
        },
        Failure::Unreachable { .. } => remote("the bucket did not answer (not asked again)"),
        Failure::TimedOut { .. } => remote("the bucket gave no answer by its deadline (not asked again)"),
        Failure::Missing { .. } => StoreError::Remote { what: "an object a signed object names is not in the bucket", status: 404 },
        Failure::Contended { .. } => remote("another pusher moved the root under each attempt"),
        Failure::LostRace { .. } => remote("the root read back is another pusher's"),
        Failure::NotConfigured { .. } => StoreError::Other { what: "the shared cache is not configured" },
        Failure::Sink(_) => StoreError::Other { what: "what the remote source fetched was not taken" },
        Failure::Misused(_) => StoreError::Other { what: "the remote source was used against its contract" },
    }
}

/// Why an object of the bucket (a root, a key list, a shard, a bundle) was refused, as a fixed
/// phrase.
fn refused(why: &Why) -> &'static str {
    match why {
        Why::Signature => "an object of the bucket refused: its signature does not verify",
        Why::UnknownSigner => "an object of the bucket refused: its signer is not trusted for its namespace",
        Why::Revoked => "an object of the bucket refused: its signer is revoked",
        Why::NotValid { .. } => "an object of the bucket refused: its signer was not valid when it was issued",
        Why::Role { .. } => "an object of the bucket refused: its signer may not sign it",
        Why::Namespace { .. } => "an object of the bucket refused: it is of another namespace",
        Why::Toolchain => "an object of the bucket refused: it is of another toolchain",
        Why::Digest => "an object of the bucket refused: its bytes are not the ones its digest names",
        Why::TooLarge { .. } => "an object of the bucket refused: it is larger than its kind may be",
        Why::TooMany { .. } => "an object of the bucket refused: it holds more items than its kind may",
        Why::Segment(_) => "an object of the bucket refused: the plain store refuses it as a segment",
        Why::Statement(_) => "an object of the bucket refused: its statement is refused",
        Why::Wire(_) => "an object of the bucket refused: it is not of its form",
        Why::Length { .. } => "an object of the bucket refused: its length is not the one named for it",
        Why::Malformed(_) => "an object of the bucket refused: it is not well formed",
        Why::OutsideSegment => "an object of the bucket refused: an index entry places a record outside its segment",
        Why::StaleKeyList { .. } => "an object of the bucket refused: a key list older than one already seen",
        Why::NotDeterministic { .. } => "an object of the bucket refused: two builders made different values under one key",
        Why::Private { .. } => "an object of the bucket refused: a segment of one namespace was to go to another",
        Why::Tier => "an object of the bucket refused: only the top tier is shared in a public namespace",
        Why::Name(_) => "an object of the bucket refused: a name part is refused",
    }
}

impl Source for Remote<'_> {
    /// Asks `namespace`'s index alone (its root, key list and shards, each checked to be of
    /// `namespace`), and answers each key from that answer only: nothing found in another
    /// namespace is ever put in `found`. The shard entry carries no tag, so a key is matched by
    /// its content id (which hashes its kind in), not by `tag`.
    fn find(&mut self, namespace: Namespace, keys: &[RecordRef], found: &mut [Option<Located>]) -> Result<(), StoreError> {
        if found.len() != keys.len() {
            return Err(StoreError::Other { what: "a find whose answer is not one slot per key" });
        }
        found.fill(None);
        let answer = self.find_entries(namespace, keys.iter().map(|it| it.key.0).collect()).map_err(|it| store_error(&it))?;
        let by_key: BTreeMap<[u8; 32], &Entry> = answer.found.iter().map(|it| (it.key, it)).collect();
        for (slot, key) in found.iter_mut().zip(keys) {
            *slot = by_key.get(&key.key.0).map(|it| Located { segment: *it.segment(), at: it.at(), len: it.payload_len() });
        }
        Ok(())
    }

    /// A segment fetched ahead (a bundle) is copied out of what is held, and let go once read
    /// whole; any other range is one ranged GET of exactly its length.
    fn fetch_into(&mut self, segment: &SegmentRef, range: Range<u64>, out: &mut [u8]) -> Result<(), StoreError> {
        let id = segment.digest();
        let len = match range.end.checked_sub(range.start) {
            Some(len) if range.end <= segment.size() && len == out.len() as u64 => len,
            _ => return Err(StoreError::Other { what: "a fetch whose range is not inside its segment, or not its buffer's length" }),
        };
        let namespace = self.located(&id)?;
        if let Some(held) = self.held.get(&id) {
            if held.bytes.len() as u64 != segment.size() {
                return Err(StoreError::Fetched { segment: id, len: held.bytes.len() as u64, wanted: segment.size() });
            }
            out.copy_from_slice(&held.bytes[range.start as usize..range.end as usize]);
            if range.start == 0 && range.end == segment.size() {
                self.held.remove(&id);
            }
            return Ok(());
        }
        let object = named(name::segment(namespace, &self.toolchain(), &id.0))?;
        let bytes = self.get(Op::Get { name: object.clone(), range: Some((range.start, len)), most: len }, &object).map_err(|it| store_error(&it))?;
        if bytes.len() as u64 != len {
            return Err(StoreError::Fetched { segment: id, len: bytes.len() as u64, wanted: len });
        }
        out.copy_from_slice(&bytes);
        Ok(())
    }

    fn statement(&mut self, segment: &SegmentRef) -> Result<Statement, StoreError> {
        let id = segment.digest();
        let namespace = self.located(&id)?;
        let decoded = match self.held.get(&id) {
            Some(held) => Statement::decode(&held.statement),
            None => {
                let object = named(name::statement(namespace, &self.toolchain(), &id.0))?;
                let bytes = self.get(Op::Get { name: object.clone(), range: None, most: Statement::LEN as u64 }, &object).map_err(|it| store_error(&it))?;
                Statement::decode(&bytes)
            }
        };
        decoded.map_err(|why| StoreError::Wire { what: "a segment's statement", why })
    }
}
