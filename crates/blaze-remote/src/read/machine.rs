//! The read path as a sans-IO machine (rule 14): given a batch of keys the command misses
//! locally, it decides which objects and byte ranges to ask an origin for, checks every byte it
//! is answered, and answers each key found (its bytes, verified), absent (the origin proves it
//! does not hold it) or not fetched (why). It owns no IO and no clock: the time comes with each
//! step, and each ask carries its deadline.
//!
//! A batch goes in waves, each wave's asks in parallel up to [`Limits::in_flight_most`]:
//!
//! 1. the head (once a command), its signature checked;
//! 2. the manifest it names (once a command), checked against the head's digest;
//! 3. each segment's summary (once a command), checked against its row: its fuse filter and its
//!    pages' first keys say, with no request, which page of which segment may hold each key;
//! 4. those index pages (each once a command), each checked against its digest in the summary;
//! 5. the payloads, read in place from the segments by range: payloads of one segment that lie
//!    within [`Limits::gap_most`] of each other are one range, up to [`Limits::range_most`];
//!    each payload is checked against its value digest before it is answered.
//!
//! With [`Limits::prefix_most`] above 0 (the published set's reader, published.md), waves 1 to 3
//! are one: the `prefix` object, the head, the manifest and every summary in one read, each
//! part checked as its own object is. A batch is then at most three waves.
//!
//! A failure (an origin's error, a short read, an ask past its deadline, a refused object) is
//! kept for the rest of the command against what it was about (the head, the manifest, or one
//! segment), so it is never asked again (no retry, rule 3); each key it leaves unknown is
//! answered [`Answer::Unfetched`] with the failure. That is the one place the command goes on
//! without what it asked for, and it is the design: reading the cache is an optimisation of a
//! local computation that makes the same bytes (rule 25), so its absence changes no output; it
//! is never silent, as the caller prints what was not fetched and why (remote-read.md).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::fmt;

use blaze_cas::plain::{Entry, Namespace};
use blaze_ids::ContentId;
use blaze_keyed::kind::Tag;
use blaze_own::{Alloc, Vec};

use super::form::{self, HEAD_LEN, Head, HeadCheck, Object, REMOTE_ENTRY, Refused, Row, Summary};

/// Every bound of the read path (rule 18): what a batch may ask, hold and wait for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Keys one batch asks for.
    pub keys_most: u32,
    /// One record's bytes: a longer one is not fetched.
    pub record_most: u32,
    /// Payload bytes one batch fetches.
    pub batch_bytes_most: u64,
    /// One coalesced range's bytes (a record longer than this is a range of its own).
    pub range_most: u32,
    /// Bytes between two payloads still read as one range.
    pub gap_most: u32,
    /// Requests one batch makes.
    pub asks_most: u32,
    /// Requests in flight at once.
    pub in_flight_most: u32,
    /// Each request's deadline, from when it is asked.
    pub ask_ns: u64,
    /// The batch's deadline, from its start: no request outlives it.
    pub batch_ns: u64,
    /// Index pages held for the command.
    pub pages_most: u32,
    /// Bytes of the `prefix` object asked in place of the head, the manifest and the summaries
    /// (published.md, "The reader's waves"): 0 asks those three one wave each, as an origin
    /// without a prefix needs; above 0 they come in one wave, so a batch is at most three
    /// (prefix, index pages, payloads). A prefix longer than this, or than `form::PREFIX_MOST`,
    /// is refused.
    pub prefix_most: u32,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        keys_most: 4096,
        record_most: 64 << 20,
        batch_bytes_most: 256 << 20,
        range_most: 1 << 20,
        gap_most: 4 << 10,
        asks_most: 4096,
        in_flight_most: 32,
        ask_ns: 2_000_000_000,
        batch_ns: 10_000_000_000,
        pages_most: 4096,
        prefix_most: 0,
    };

    /// The published set's reader: [`Limits::DEFAULT`] with the prefix asked, 1 MiB of it.
    pub const PUBLISHED: Limits = Limits { prefix_most: 1 << 20, ..Limits::DEFAULT };
}

/// How an origin failed one read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OriginError {
    /// It holds no object of that name.
    Missing,
    /// It failed the read: its IO, or its own wait stopped.
    Unreadable,
}

/// Why a key was not fetched: each names the object it was about (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unfetched {
    Origin { object: Object, error: OriginError },
    /// The origin answered fewer bytes than asked.
    Short { object: Object, got: u64, wanted: u64 },
    /// No answer by the ask's deadline.
    Late { object: Object, waited_ns: u64 },
    Refused(Refused),
    /// A length past a bound of this reader, refused before anything was asked or allocated.
    Limit { what: &'static str, len: u64, most: u64 },
}

impl fmt::Display for Unfetched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unfetched::Origin { object, error: OriginError::Missing } => write!(f, "{object}: the origin holds no such object"),
            Unfetched::Origin { object, error: OriginError::Unreadable } => write!(f, "{object}: the origin failed the read"),
            Unfetched::Short { object, got, wanted } => write!(f, "{object}: the origin answered {got} of {wanted} bytes"),
            Unfetched::Late { object, waited_ns } => write!(f, "{object}: no answer in {} ms", waited_ns / 1_000_000),
            Unfetched::Refused(refused) => write!(f, "{refused}"),
            Unfetched::Limit { what, len, most } => write!(f, "{what} is {len} bytes, past the {most} a batch takes"),
        }
    }
}

/// One request for an origin: `len` bytes of `object` from `at`, wanted by `deadline_ns`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ask {
    pub id: u32,
    pub object: Object,
    pub at: u64,
    pub len: u32,
    pub deadline_ns: u64,
}

/// What a batch answered for one key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Not answered yet.
    Pending,
    /// Its bytes, checked: [`Reader::found`].
    Found { range: u32, at: u32, len: u32, tag: Tag },
    /// The origin holds no record of this key: every segment's filter, fences or page says so.
    Absent,
    Unfetched(Unfetched),
}

/// What [`Reader::poll`] says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Poll {
    /// Asks are in flight: call again with a reply, or at `until` at the latest.
    Waiting { until: u64 },
    /// Every key of the batch is answered.
    Done,
}

/// Why a batch was not begun.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BatchError {
    TooManyKeys { keys: u64, most: u32 },
    /// A batch was begun before the last one was done: the caller's bug.
    Busy,
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchError::TooManyKeys { keys, most } => write!(f, "a batch of {keys} keys, past the {most} the read path takes"),
            BatchError::Busy => f.write_str("a batch begun while another was in flight"),
        }
    }
}

/// Counts over the command, for its profile line.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Report {
    pub batches: u32,
    pub keys: u32,
    pub found: u32,
    pub absent: u32,
    pub unfetched: u32,
    pub asks: u32,
    pub bytes_fetched: u64,
    /// Bytes of records answered found.
    pub bytes_used: u64,
    /// Replies that came after their ask's deadline, not used.
    pub late: u32,
    pub head_asks: u32,
    pub manifest_asks: u32,
    pub summary_asks: u32,
    pub page_asks: u32,
    pub payload_asks: u32,
    pub prefix_asks: u32,
    /// Round trips: waves that made at least one ask (each wave's asks go out together, up to
    /// `in_flight_most` at once).
    pub waves: u32,
}

enum Held<T> {
    No,
    Yes(T),
    Failed(Unfetched),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want {
    Head,
    Prefix,
    Manifest,
    Summary(u32),
    Page { row: u32, page: u32 },
    Payload(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Head,
    Prefix,
    Manifest,
    Summaries,
    Pages,
    Payloads,
    Done,
}

struct Slot<'a> {
    ask: Ask,
    want: Want,
    asked_ns: u64,
    buf: Vec<u8, Alloc<'a>>,
}

struct Page<'a> {
    row: u32,
    page: u32,
    bytes: Vec<u8, Alloc<'a>>,
}

#[derive(Clone, Copy)]
struct Probe {
    key: u32,
    row: u32,
    page: u32,
}

#[derive(Clone, Copy)]
struct Part {
    key: u32,
    row: u32,
    entry: Entry,
    digest: ContentId,
}

struct Range<'a> {
    row: u32,
    at: u64,
    /// Its parts: `parts[first..end]`.
    first: u32,
    end: u32,
    bytes: Vec<u8, Alloc<'a>>,
}

/// The read path for one command against one origin: what it holds of the origin (head,
/// manifest, summaries, pages) lives as long as it, so a later batch asks only what is new.
pub struct Reader<'a> {
    alloc: Alloc<'a>,
    limits: Limits,
    namespace: Namespace,
    check: &'a dyn HeadCheck,
    head: Held<Head>,
    manifest: Held<()>,
    rows: Vec<Row, Alloc<'a>>,
    summaries: Vec<Held<Summary<'a>>, Alloc<'a>>,
    pages: Vec<Page<'a>, Alloc<'a>>,
    // The batch.
    phase: Phase,
    began_ns: u64,
    keys: Vec<ContentId, Alloc<'a>>,
    answers: Vec<Answer, Alloc<'a>>,
    doubt: Vec<Option<Unfetched>, Alloc<'a>>,
    probes: Vec<Probe, Alloc<'a>>,
    parts: Vec<Part, Alloc<'a>>,
    ranges: Vec<Range<'a>, Alloc<'a>>,
    queue: Vec<(Want, Object, u64, u32), Alloc<'a>>,
    /// The queue's asks before this one are made.
    queued: usize,
    slots: Vec<Slot<'a>, Alloc<'a>>,
    batch_asks: u32,
    next_id: u32,
    report: Report,
}

impl<'a> Reader<'a> {
    pub fn new_in(namespace: Namespace, check: &'a dyn HeadCheck, limits: Limits, alloc: Alloc<'a>) -> Reader<'a> {
        // At least one ask in flight, or no batch could end.
        let limits = Limits { in_flight_most: limits.in_flight_most.max(1), ..limits };
        Reader {
            alloc,
            limits,
            namespace,
            check,
            head: Held::No,
            manifest: Held::No,
            rows: Vec::new_in(alloc),
            summaries: Vec::new_in(alloc),
            pages: Vec::new_in(alloc),
            phase: Phase::Idle,
            began_ns: 0,
            keys: Vec::new_in(alloc),
            answers: Vec::new_in(alloc),
            doubt: Vec::new_in(alloc),
            probes: Vec::new_in(alloc),
            parts: Vec::new_in(alloc),
            ranges: Vec::new_in(alloc),
            queue: Vec::new_in(alloc),
            queued: 0,
            slots: Vec::new_in(alloc),
            batch_asks: 0,
            next_id: 0,
            report: Report::default(),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn report(&self) -> Report {
        self.report
    }

    /// The head, once checked: who signed what is read.
    pub fn head(&self) -> Option<&Head> {
        match &self.head {
            Held::Yes(head) => Some(head),
            Held::No | Held::Failed(_) => None,
        }
    }

    /// Begins a batch of `keys` (any order, repeats allowed); answers are by position.
    pub fn begin(&mut self, keys: &[ContentId], now: u64) -> Result<(), BatchError> {
        if !matches!(self.phase, Phase::Idle | Phase::Done) {
            return Err(BatchError::Busy);
        }
        if keys.len() > self.limits.keys_most as usize {
            return Err(BatchError::TooManyKeys { keys: keys.len() as u64, most: self.limits.keys_most });
        }
        self.keys.clear();
        self.keys.extend_from_slice(keys);
        self.answers.clear();
        self.answers.resize(keys.len(), Answer::Pending);
        self.doubt.clear();
        self.doubt.resize(keys.len(), None);
        self.probes.clear();
        self.parts.clear();
        self.ranges.clear();
        self.queue.clear();
        self.queued = 0;
        self.batch_asks = 0;
        self.began_ns = now;
        self.phase = Phase::Idle;
        self.report.batches += 1;
        self.report.keys += keys.len() as u32;
        Ok(())
    }

    /// The answer for the `i`th key of the batch.
    pub fn answer(&self, i: usize) -> Answer {
        self.answers[i]
    }

    /// The bytes of a [`Answer::Found`], checked against their digest.
    pub fn found(&self, i: usize) -> Option<&[u8]> {
        match self.answers[i] {
            Answer::Found { range, at, len, .. } => Some(&self.ranges[range as usize].bytes[at as usize..at as usize + len as usize]),
            Answer::Pending | Answer::Absent | Answer::Unfetched(_) => None,
        }
    }

    /// The buffer an ask's reply is written into, `len` bytes: `None` when the ask is no longer
    /// in flight (answered, or past its deadline).
    pub fn buffer(&mut self, id: u32) -> Option<&mut [u8]> {
        self.slots.iter_mut().find(|slot| slot.ask.id == id).map(|slot| slot.buf.as_mut_slice())
    }

    /// Steps the batch at `now`: expires asks past their deadline, moves to the next wave when
    /// this one is answered, and puts the asks to make into `out`.
    pub fn poll(&mut self, now: u64, out: &mut Vec<Ask, Alloc<'_>>) -> Poll {
        self.expire(now);
        while self.slots.is_empty() && self.queued == self.queue.len() {
            if self.phase == Phase::Done {
                return Poll::Done;
            }
            self.queue.clear();
            self.queued = 0;
            self.advance();
            if !self.queue.is_empty() {
                self.report.waves += 1;
            }
        }
        let batch_deadline = self.began_ns.saturating_add(self.limits.batch_ns);
        while self.slots.len() < self.limits.in_flight_most as usize && self.queued < self.queue.len() {
            let (want, object, at, len) = self.queue[self.queued];
            self.queued += 1;
            let ask = Ask { id: self.next_id, object, at, len, deadline_ns: now.saturating_add(self.limits.ask_ns).min(batch_deadline) };
            self.next_id = self.next_id.wrapping_add(1);
            let mut buf = Vec::with_capacity_in(len as usize, self.alloc);
            buf.resize(len as usize, 0);
            self.slots.push(Slot { ask, want, asked_ns: now, buf });
            self.report.asks += 1;
            match want {
                Want::Head => self.report.head_asks += 1,
                Want::Prefix => self.report.prefix_asks += 1,
                Want::Manifest => self.report.manifest_asks += 1,
                Want::Summary(_) => self.report.summary_asks += 1,
                Want::Page { .. } => self.report.page_asks += 1,
                Want::Payload(_) => self.report.payload_asks += 1,
            }
            out.push(ask);
        }
        let until = self.slots.iter().map(|slot| slot.ask.deadline_ns).min().unwrap_or(now);
        Poll::Waiting { until }
    }

    /// The reply to ask `id`: `Ok(n)` when the origin wrote `n` bytes into its [`buffer`]
    /// (Self::buffer). A reply to an ask no longer in flight, or after its deadline, is not used.
    pub fn replied(&mut self, id: u32, result: Result<usize, OriginError>, now: u64) {
        let Some(at) = self.slots.iter().position(|slot| slot.ask.id == id) else {
            self.report.late += 1;
            return;
        };
        let mut slot = self.slots.remove_swap(at);
        let object = slot.ask.object;
        let outcome = if now > slot.ask.deadline_ns {
            self.report.late += 1;
            Err(Unfetched::Late { object, waited_ns: now - slot.asked_ns })
        } else {
            match result {
                Err(error) => Err(Unfetched::Origin { object, error }),
                // The prefix is asked for up to `prefix_most` bytes, so fewer is its end: its
                // decode then holds it to the length its head and rows say.
                Ok(got) if slot.want == Want::Prefix && got <= slot.buf.len() => {
                    slot.buf.truncate(got);
                    self.report.bytes_fetched += got as u64;
                    Ok(())
                }
                Ok(got) if got != slot.buf.len() => Err(Unfetched::Short { object, got: got as u64, wanted: slot.buf.len() as u64 }),
                Ok(got) => {
                    self.report.bytes_fetched += got as u64;
                    Ok(())
                }
            }
        };
        self.take(slot.want, slot.buf, outcome);
    }

    fn expire(&mut self, now: u64) {
        let mut i = 0;
        while i < self.slots.len() {
            if self.slots[i].ask.deadline_ns <= now {
                let slot = self.slots.remove_swap(i);
                let object = slot.ask.object;
                self.take(slot.want, slot.buf, Err(Unfetched::Late { object, waited_ns: now - slot.asked_ns }));
            } else {
                i += 1;
            }
        }
    }

    /// One reply, checked, into what the reader holds.
    fn take(&mut self, want: Want, bytes: Vec<u8, Alloc<'a>>, outcome: Result<(), Unfetched>) {
        match want {
            Want::Head => {
                self.head = match outcome.and_then(|()| Head::decode(&bytes, self.namespace, self.check).map_err(Unfetched::Refused)) {
                    Ok(head) => Held::Yes(head),
                    Err(why) => Held::Failed(why),
                };
            }
            Want::Prefix => {
                let most = u64::from(self.limits.prefix_most);
                match outcome.and_then(|()| form::decode_prefix(&bytes, most, self.namespace, self.check, self.alloc).map_err(Unfetched::Refused)) {
                    Ok(prefixed) => {
                        self.head = Held::Yes(prefixed.head);
                        self.manifest = Held::Yes(());
                        self.rows = prefixed.rows;
                        self.summaries.clear();
                        for summary in prefixed.summaries {
                            self.summaries.push(Held::Yes(summary));
                        }
                    }
                    // What the head would have been: the manifest and summaries come only
                    // through it, so the batch fails as a failed head does.
                    Err(why) => self.head = Held::Failed(why),
                }
            }
            Want::Manifest => {
                let Held::Yes(head) = &self.head else { return };
                let head = *head;
                let mut rows = Vec::new_in(self.alloc);
                self.manifest = match outcome.and_then(|()| form::decode_manifest(&bytes, &head, &mut rows).map_err(Unfetched::Refused)) {
                    Ok(()) => {
                        self.summaries.clear();
                        for _ in 0..rows.len() {
                            self.summaries.push(Held::No);
                        }
                        self.rows = rows;
                        Held::Yes(())
                    }
                    Err(why) => Held::Failed(why),
                };
            }
            Want::Summary(row) => {
                let held = match outcome.and_then(|()| Summary::decode(&bytes, &self.rows[row as usize], self.alloc).map_err(Unfetched::Refused)) {
                    Ok(summary) => Held::Yes(summary),
                    Err(why) => Held::Failed(why),
                };
                self.summaries[row as usize] = held;
            }
            Want::Page { row, page } => {
                let checked = outcome.and_then(|()| match &self.summaries[row as usize] {
                    Held::Yes(summary) if form::page_digest(&bytes) == summary.page_digest(page) => Ok(()),
                    Held::Yes(_) => Err(Unfetched::Refused(Refused::Digest { object: Object::Index(self.rows[row as usize].segment) })),
                    Held::No | Held::Failed(_) => Err(Unfetched::Refused(Refused::Digest { object: Object::Index(self.rows[row as usize].segment) })),
                });
                match checked {
                    Ok(()) => self.pages.push(Page { row, page, bytes }),
                    Err(why) => self.fail_row(row, why),
                }
            }
            Want::Payload(range) => {
                let range = range as usize;
                match outcome {
                    Ok(()) => {
                        self.ranges[range].bytes = bytes;
                        let (first, end, at) = (self.ranges[range].first, self.ranges[range].end, self.ranges[range].at);
                        // Ranges are laid in segment order, so a range after one of its own
                        // segment is that segment's second or later.
                        let later_of_its_segment = range > 0 && self.ranges[range - 1].row == self.ranges[range].row;
                        let unverified = crate::planted(crate::Plant::SecondRangeUnverified) && later_of_its_segment;
                        for p in first..end {
                            let part = self.parts[p as usize];
                            let start = (part.entry.at - at) as usize;
                            let payload = &self.ranges[range].bytes[start..start + part.entry.len as usize];
                            let checked = crate::planted(crate::Plant::PayloadBeforeDigest)
                                || unverified
                                || crate::segment::value_digest(part.entry.tag, payload) == part.digest;
                            self.answers[part.key as usize] = if checked {
                                Answer::Found { range: range as u32, at: start as u32, len: part.entry.len, tag: part.entry.tag }
                            } else {
                                Answer::Unfetched(Unfetched::Refused(Refused::Digest { object: Object::Segment(self.rows[part.row as usize].segment) }))
                            };
                        }
                    }
                    Err(why) => {
                        let (first, end, row) = (self.ranges[range].first, self.ranges[range].end, self.ranges[range].row);
                        for p in first..end {
                            self.answers[self.parts[p as usize].key as usize] = Answer::Unfetched(why);
                        }
                        self.fail_row(row, why);
                    }
                }
            }
        }
    }

    /// A segment that failed is not read again this command.
    fn fail_row(&mut self, row: u32, why: Unfetched) {
        self.summaries[row as usize] = Held::Failed(why);
        self.pages.retain(|page| page.row != row);
    }

    fn want(&mut self, want: Want, object: Object, at: u64, len: u32) -> Result<(), Unfetched> {
        if self.batch_asks >= self.limits.asks_most {
            return Err(Unfetched::Limit { what: "the batch's requests", len: u64::from(self.batch_asks) + 1, most: u64::from(self.limits.asks_most) });
        }
        self.batch_asks += 1;
        self.queue.push((want, object, at, len));
        Ok(())
    }

    /// Every key not yet answered, answered `why`.
    fn fail_all(&mut self, why: Unfetched) {
        for answer in self.answers.iter_mut() {
            if *answer == Answer::Pending {
                *answer = Answer::Unfetched(why);
            }
        }
        self.finish();
    }

    /// The batch is answered: its counts into the command's.
    fn finish(&mut self) {
        for answer in self.answers.iter_mut() {
            if *answer == Answer::Pending {
                // A key no wave answered: every path answers each key, so this is the reader's
                // own bug, named.
                *answer = Answer::Unfetched(Unfetched::Limit { what: "a key no wave answered", len: 0, most: 0 });
            }
        }
        for answer in self.answers.iter() {
            match answer {
                Answer::Found { len, .. } => {
                    self.report.found += 1;
                    self.report.bytes_used += u64::from(*len);
                }
                Answer::Absent => self.report.absent += 1,
                Answer::Unfetched(_) => self.report.unfetched += 1,
                Answer::Pending => {}
            }
        }
        self.phase = Phase::Done;
    }

    /// The wave just answered is taken; the next one's asks are queued.
    fn advance(&mut self) {
        match self.phase {
            Phase::Idle if self.keys.is_empty() => self.finish(),
            Phase::Idle if self.limits.prefix_most > 0 && matches!(self.head, Held::No) => {
                self.phase = Phase::Prefix;
                // A bound of this reader, not a length from the wire.
                let most = u64::from(self.limits.prefix_most).min(form::PREFIX_MOST) as u32;
                let _ = self.want(Want::Prefix, Object::Prefix, 0, most);
            }
            Phase::Prefix => match &self.head {
                Held::Failed(why) => self.fail_all(*why),
                Held::No => self.fail_all(Unfetched::Late { object: Object::Prefix, waited_ns: 0 }),
                // The head, the manifest and every summary are held: the next turn probes.
                Held::Yes(_) => self.phase = Phase::Summaries,
            },
            Phase::Idle => {
                self.phase = Phase::Head;
                if matches!(self.head, Held::No) {
                    // The head's length is fixed: nothing from the wire sizes this ask.
                    let _ = self.want(Want::Head, Object::Head, 0, HEAD_LEN as u32);
                }
            }
            Phase::Head => match &self.head {
                Held::Failed(why) => self.fail_all(*why),
                Held::No => self.fail_all(Unfetched::Late { object: Object::Head, waited_ns: 0 }),
                Held::Yes(head) => {
                    let head = *head;
                    self.phase = Phase::Manifest;
                    if matches!(self.manifest, Held::No) {
                        // `manifest_len` is bounded by `Head::decode` (MANIFEST_MOST).
                        let _ = self.want(Want::Manifest, Object::Manifest(head.manifest), 0, head.manifest_len as u32);
                    }
                }
            },
            Phase::Manifest => match &self.manifest {
                Held::Failed(why) => self.fail_all(*why),
                Held::No => self.fail_all(Unfetched::Late { object: Object::Head, waited_ns: 0 }),
                Held::Yes(()) => {
                    self.phase = Phase::Summaries;
                    for row in 0..self.rows.len() {
                        if matches!(self.summaries[row], Held::No) {
                            let (segment, len) = (self.rows[row].segment, self.rows[row].summary_len);
                            // `summary_len` is bounded by the row's decode (SUMMARY_MOST).
                            if let Err(why) = self.want(Want::Summary(row as u32), Object::Summary(segment), 0, len as u32) {
                                self.summaries[row] = Held::Failed(why);
                            }
                        }
                    }
                }
            },
            Phase::Summaries => {
                self.phase = Phase::Pages;
                self.probe();
            }
            Phase::Pages => {
                self.phase = Phase::Payloads;
                self.lookup();
            }
            Phase::Payloads => self.finish(),
            Phase::Done => {}
        }
    }

    /// Wave 4: for each key, the pages that may hold it, from the filters and fences alone.
    fn probe(&mut self) {
        self.probes.clear();
        for key in 0..self.keys.len() {
            for row in 0..self.rows.len() {
                match &self.summaries[row] {
                    Held::Yes(summary) => {
                        if let Some(page) = summary.page_of(&self.keys[key]) {
                            self.probes.push(Probe { key: key as u32, row: row as u32, page });
                        }
                    }
                    Held::Failed(why) => {
                        if self.doubt[key].is_none() {
                            self.doubt[key] = Some(*why);
                        }
                    }
                    Held::No => {}
                }
            }
        }
        // Each page once, in segment and page order.
        let mut wanted: Vec<(u32, u32), Alloc<'_>> = Vec::with_capacity_in(self.probes.len(), self.alloc);
        for probe in self.probes.iter() {
            if !self.pages.iter().any(|page| page.row == probe.row && page.page == probe.page) {
                wanted.push((probe.row, probe.page));
            }
        }
        wanted.sort_unstable();
        wanted.dedup();
        // The pages held stay within their bound: those this batch does not need go first.
        let room = self.limits.pages_most as usize;
        if self.pages.len() + wanted.len() > room {
            let probes = &self.probes;
            self.pages.retain(|page| probes.iter().any(|probe| probe.row == page.row && probe.page == page.page));
        }
        for (i, &(row, page)) in wanted.iter().enumerate() {
            let r = self.rows[row as usize];
            if self.pages.len() + i >= room {
                self.fail_row(row, Unfetched::Limit { what: "the index pages held", len: (self.pages.len() + i + 1) as u64, most: u64::from(self.limits.pages_most) });
                continue;
            }
            let at = u64::from(page) * u64::from(r.page_entries) * REMOTE_ENTRY as u64;
            // A page is at most PAGE_ENTRIES_MOST entries (the row's decode): 64 KiB.
            let len = r.page_len(page) * REMOTE_ENTRY as u32;
            if let Err(why) = self.want(Want::Page { row, page }, Object::Index(r.segment), at, len) {
                self.fail_row(row, why);
            }
        }
    }

    /// Wave 5: each key's entry from its pages, then the payload ranges.
    fn lookup(&mut self) {
        self.parts.clear();
        let mut seen: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(self.keys.len(), self.alloc);
        seen.resize(self.keys.len(), false);
        for p in 0..self.probes.len() {
            let probe = self.probes[p];
            if seen[probe.key as usize] {
                continue;
            }
            let row = self.rows[probe.row as usize];
            if let Held::Failed(why) = &self.summaries[probe.row as usize] {
                if self.doubt[probe.key as usize].is_none() {
                    self.doubt[probe.key as usize] = Some(*why);
                }
                continue;
            }
            let Some(page) = self.pages.iter().find(|page| page.row == probe.row && page.page == probe.page) else { continue };
            match form::lookup(&page.bytes, &row, probe.page, &self.keys[probe.key as usize]) {
                Ok(Some((entry, digest))) => {
                    seen[probe.key as usize] = true;
                    self.parts.push(Part { key: probe.key, row: probe.row, entry, digest });
                }
                Ok(None) => {}
                Err(refused) => {
                    self.fail_row(probe.row, Unfetched::Refused(refused));
                    if self.doubt[probe.key as usize].is_none() {
                        self.doubt[probe.key as usize] = Some(Unfetched::Refused(refused));
                    }
                }
            }
        }
        // A key that is in no segment (and no segment left in doubt) is absent.
        for key in 0..self.keys.len() {
            if !seen[key] {
                self.answers[key] = match self.doubt[key] {
                    Some(why) => Answer::Unfetched(why),
                    None => Answer::Absent,
                };
            }
        }
        // Bounds before any byte is asked: a record past `record_most`, and the batch's bytes.
        let mut total = 0u64;
        let limits = self.limits;
        let answers = &mut self.answers;
        self.parts.retain(|part| {
            let len = u64::from(part.entry.len);
            if !crate::planted(crate::Plant::SizeFromWire) && len > u64::from(limits.record_most) {
                answers[part.key as usize] = Answer::Unfetched(Unfetched::Limit { what: "a record", len, most: u64::from(limits.record_most) });
                return false;
            }
            if total + len > limits.batch_bytes_most {
                answers[part.key as usize] = Answer::Unfetched(Unfetched::Limit { what: "the batch's records", len: total + len, most: limits.batch_bytes_most });
                return false;
            }
            total += len;
            true
        });
        self.parts.sort_unstable_by_key(|part| (part.row, part.entry.at));
        // Coalesce: one range while the next payload starts within `gap_most` of this range's
        // end and the range stays within `range_most`.
        let mut p = 0;
        while p < self.parts.len() {
            let first = self.parts[p];
            let (row, at) = (first.row, first.entry.at);
            let mut end = at + u64::from(first.entry.len);
            let mut q = p + 1;
            while q < self.parts.len() {
                let next = self.parts[q];
                let next_end = next.entry.at + u64::from(next.entry.len);
                if next.row != row || next.entry.at > end + u64::from(limits.gap_most) || next_end.max(end) - at > u64::from(limits.range_most) {
                    break;
                }
                end = end.max(next_end);
                q += 1;
            }
            let range = self.ranges.len() as u32;
            let (ask_at, ask_len) = if crate::planted(crate::Plant::RangeOffByOne) { (at + 1, end - at) } else { (at, end - at) };
            let segment = self.rows[row as usize].segment;
            // `at` is where the parts are measured from; the ask is what goes to the origin.
            self.ranges.push(Range { row, at, first: p as u32, end: q as u32, bytes: Vec::new_in(self.alloc) });
            if let Err(why) = self.want(Want::Payload(range), Object::Segment(segment), ask_at, ask_len as u32) {
                for part in &self.parts[p..q] {
                    self.answers[part.key as usize] = Answer::Unfetched(why);
                }
            }
            p = q;
        }
    }
}
