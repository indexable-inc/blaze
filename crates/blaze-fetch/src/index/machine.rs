//! The index machine (sans-IO, rule 14): a batch of asks in, effects out, completions in, each
//! ask's index files and how each ended out. It owns no socket, file or clock: the time comes
//! with each call and each request carries its deadline (rules 15, 18, 44 are the driver's).
//!
//! A batch goes:
//!
//! 1. Each ask names the files it needs: its crate's, or for [`QueryKind::AlternativeNames`]
//!    every spelling of it with `-` and `_` swapped (cargo's `UncanonicalizedIter`, at most
//!    [`MOST_SPELLINGS`]). Files are told apart by their index path (lower case), so two asks
//!    of one crate in a batch, in any case, are one file and one request.
//! 2. [`IndexEffect::Read`] for every file: the driver reads its cache record (where:
//!    [`super::request::cache_path_in`]) and answers [`Index::read`] with its bytes or none.
//!    A record is opened in place ([`IndexView::open`]) for its validator; a record of an old
//!    form is counted ([`Index::rebuilt`]) and asked for whole; a damaged one fails its file.
//!    Under [`Update::CacheFirst`] a file whose record opened ends here, asking nothing.
//! 3. Once every read is in, ONE batch of [`IndexEffect::Http`], at most
//!    [`Limits::in_flight_most`] at once: each with the validator as its condition
//!    (`If-None-Match` or `If-Modified-Since`), a byte limit and a deadline.
//! 4. A 200 is parsed ([`super::line::parse_in`]) into a new record, which the driver keeps
//!    by [`IndexEffect::Write`] and answers [`Index::written`]; a 304 keeps the record read,
//!    never parsed again; a 404, 410 or 451 is "no such crate", an answer. Any other answer or
//!    failure (no connection, a reset, a timeout, 429, 5xx included) fails its file with a
//!    typed [`IndexWhy`] at once: one try, as the fetch machine tries (D172, rule 3); the
//!    caller's next command asks again.
//! 5. [`Index::is_done`]: each ask's files ([`Index::files_of`]) and how each ended
//!    ([`Index::end`]): the record read ([`FileEnd::Cached`]), the record written
//!    ([`FileEnd::Fetched`]), no such crate ([`FileEnd::Absent`]) or why not
//!    ([`FileEnd::Failed`]).
//!
//! The answer is plain rows, read in place from the records ([`IndexView`]): blaze-resolve
//! pulls in serde and url, which this crate does not take (D123), so the summaries its
//! resolver takes are made from the rows by the driver that connects the two machines.

#![blaze::sans_io]

use blaze_own::{Alloc, String, Vec, VecDeque};
use blaze_text::{Put, Show, Writes as _};

use super::line::{self, IndexLimits, IndexRefused};
use super::record::{IndexView, Validator, ViewRefused};
use super::request::{self, AnswerKind, INDEX_HEADERS, NameRefused, RegistryKey};
use crate::fetch::{Limits, NetFailure, StoreFailure};

/// The most spellings of one name an [`QueryKind::AlternativeNames`] ask reads (cargo's bound
/// on `UncanonicalizedIter`).
pub const MOST_SPELLINGS: u32 = 1024;

/// How an ask matches: blaze-resolve's `QueryKind`, variant for variant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum QueryKind {
    Exact,
    RejectedVersions,
    /// Every spelling with `-` and `_` swapped: one file each.
    AlternativeNames,
    Normalized,
}

/// One query of the registry: a crate's name and how it matches (blaze-resolve's
/// `resolving::Ask`, its dependency named by `package_name()`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexAsk<'n> {
    pub name: &'n str,
    pub kind: QueryKind,
}

/// Whether a file held in the cache is asked about again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Update {
    /// Every file is asked for once, conditionally: cargo when it updates the index.
    Check,
    /// A file whose record is held stands, asking nothing: cargo when every package it is asked
    /// about is locked, and offline.
    CacheFirst,
}

/// Which request of a file an answer is to: a later request of the same file has another
/// `seq`, so an answer after its deadline is told apart and ignored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileAsk {
    pub file: u32,
    pub seq: u32,
}

/// What the machine asks its driver to do.
#[derive(Debug)]
pub enum IndexEffect<'a> {
    /// Read the cache record of `file` ([`Index::path`] under the registry's folder): answer
    /// with [`Index::read`].
    Read { file: u32 },
    /// A GET of `url` with `headers` and, when there is one, `condition`: answer with
    /// [`Index::answered`], giving the answer's `ETag` and `Last-Modified`. A body past `most`
    /// bytes is not read ([`NetFailure::TooBig`]); `deadline_ns` is on the machine's clock.
    Http {
        ask: FileAsk,
        url: String<Alloc<'a>>,
        headers: &'static [(&'static str, &'static str)],
        condition: Option<(&'static str, String<Alloc<'a>>)>,
        most: u64,
        deadline_ns: u64,
    },
    /// Keep `record` as `file`'s cache record, whole or not at all (the directory grant's
    /// `publish`), and keep its bytes for the answer: answer with [`Index::written`].
    Write { file: u32, record: Vec<u8, Alloc<'a>> },
}

/// An answer to [`IndexEffect::Http`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexReply<'b> {
    /// The final answer, its whole body and the two headers the protocol reads.
    Body { status: u16, etag: Option<&'b str>, last_modified: Option<&'b str>, bytes: &'b [u8] },
    Failed(NetFailure),
}

/// Why a file has no record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IndexWhy {
    /// The registry answered `status` to the one try (D172).
    Status { status: u16 },
    /// The one try had no answer (D172).
    Net { failure: NetFailure },
    /// A 401: the registry wants credentials, which blaze sends none of yet.
    Unauthorized,
    /// A body longer than its limit came anyway.
    TooBig { most: u64, got: u64 },
    /// A 304 to a request that asked with no validator: the registry's mistake.
    NotModifiedUnasked,
    /// The file's lines are not an index's.
    Refused(IndexRefused),
    /// The cache record is damaged.
    Cache(ViewRefused),
    /// The cache did not keep the new record.
    Store(StoreFailure),
}

impl Show for IndexWhy {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            IndexWhy::Status { status } => out.str("the registry answered ").int(*status),
            IndexWhy::Net { failure } => out.str(failure.name()),
            IndexWhy::Unauthorized => out.str("the registry wants credentials (401), and blaze sends none"),
            IndexWhy::TooBig { most, got } => out.str("an index file of ").byte_count(*got).str(", past its limit of ").byte_count(*most),
            IndexWhy::NotModifiedUnasked => out.str("the registry answered 304 to a request with no validator"),
            IndexWhy::Refused(why) => out.str("its index file: ").show(why),
            IndexWhy::Cache(why) => out.str("its cache record: ").show(why),
            IndexWhy::Store(StoreFailure::Io) => out.str("its cache record was not kept: a write failed"),
            IndexWhy::Store(StoreFailure::Full) => out.str("its cache record was not kept: the disk is full"),
        };
    }
}

/// How a file ended, or that it has not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileEnd {
    Pending,
    /// The record the driver read stands (a 304, or [`Update::CacheFirst`]).
    Cached,
    /// The record of the last [`IndexEffect::Write`] for the file.
    Fetched,
    /// The registry has no such crate: no versions.
    Absent,
    Failed(IndexWhy),
}

/// A batch the machine refuses to start: the driver's mistake, named (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The index URL is not `sparse+https://…/`.
    Url,
    /// Ask `ask`'s name has no index file.
    Name { ask: u32, why: NameRefused },
    /// More files than a `u32` counts.
    TooManyFiles,
}

impl Show for Refused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            Refused::Url => out.str("an index URL that is not `sparse+https://…/`"),
            Refused::Name { ask, why } => out.str("ask ").int(*ask).str(": ").show(why),
            Refused::TooManyFiles => out.str("more index files than a batch counts"),
        };
    }
}

/// A bug planted by a test, to show its oracle catches it: only ever in this crate's tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexPlant {
    None,
    /// A request carries no condition though the cache holds the file's validator.
    NoValidator,
}

#[cfg(test)]
thread_local! {
    pub(crate) static INDEX_PLANT: core::cell::Cell<IndexPlant> = const { core::cell::Cell::new(IndexPlant::None) };
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(test)]
fn index_planted(plant: IndexPlant) -> bool {
    INDEX_PLANT.get() == plant
}

/// Whether `plant` is planted: never, outside tests.
#[cfg(not(test))]
#[inline(always)]
fn index_planted(_plant: IndexPlant) -> bool {
    false
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Its cache record asked for.
    Reading,
    Ready,
    Asked { seq: u32, deadline: u64 },
    /// Its new record handed to the driver to keep.
    Writing,
    Done,
}

/// A validator kept from a cache record, in the machine's allocator.
enum Held<'a> {
    None,
    Etag(String<Alloc<'a>>),
    LastModified(String<Alloc<'a>>),
}

struct File<'a> {
    /// The name in lower case, as its file is named.
    spelling: String<Alloc<'a>>,
    /// Its index path (`se/rd/serde`).
    path: String<Alloc<'a>>,
    held: Held<'a>,
    /// A cache record opened for it.
    cached: bool,
    phase: Phase,
    seq: u32,
    end: FileEnd,
}

impl File<'_> {
    fn validator(&self) -> Validator<'_> {
        match &self.held {
            Held::None => Validator::None,
            Held::Etag(tag) => Validator::Etag(tag.as_str()),
            Held::LastModified(time) => Validator::LastModified(time.as_str()),
        }
    }
}

/// The machine: see the module.
pub struct Index<'a> {
    /// The index's URL less `sparse+`, ending in `/`.
    base: String<Alloc<'a>>,
    registry: RegistryKey,
    update: Update,
    limits: Limits,
    file_limits: IndexLimits,
    files: Vec<File<'a>, Alloc<'a>>,
    /// Each ask's run in `ask_files`: its first and how many.
    asks: Vec<(u32, u32), Alloc<'a>>,
    ask_files: Vec<u32, Alloc<'a>>,
    reads: VecDeque<u32, Alloc<'a>>,
    ready: VecDeque<u32, Alloc<'a>>,
    writes: VecDeque<(u32, Vec<u8, Alloc<'a>>), Alloc<'a>>,
    asked: Vec<u32, Alloc<'a>>,
    reads_left: usize,
    left: usize,
    stale: u64,
    fetched_bytes: u64,
    parsed: u32,
    rebuilt: u32,
    alloc: Alloc<'a>,
}

impl<'a> Index<'a> {
    /// A machine over `asks` of the registry whose index is `index_url`
    /// (`sparse+https://index.crates.io/`, [`request::CRATES_IO_INDEX`]).
    pub fn new(
        index_url: &str,
        asks: &[IndexAsk<'_>],
        update: Update,
        limits: Limits,
        file_limits: IndexLimits,
        alloc: Alloc<'a>,
    ) -> Result<Index<'a>, Refused> {
        let Some(base) = index_url.strip_prefix("sparse+").filter(|it| it.starts_with("https://") && it.ends_with('/')) else {
            return Err(Refused::Url);
        };
        let mut index = Index {
            base: String::from_str_in(base, alloc),
            registry: RegistryKey::of(index_url),
            update,
            limits,
            file_limits,
            files: Vec::new_in(alloc),
            asks: Vec::with_capacity_in(asks.len(), alloc),
            ask_files: Vec::with_capacity_in(asks.len(), alloc),
            reads: VecDeque::new_in(alloc),
            ready: VecDeque::new_in(alloc),
            writes: VecDeque::new_in(alloc),
            asked: Vec::new_in(alloc),
            reads_left: 0,
            left: 0,
            stale: 0,
            fetched_bytes: 0,
            parsed: 0,
            rebuilt: 0,
            alloc,
        };
        let mut spelling = String::new_in(alloc);
        for (at, ask) in asks.iter().enumerate() {
            let refused = |why| Refused::Name { ask: at as u32, why };
            request::check_name(ask.name).map_err(refused)?;
            let first = index.ask_files.len() as u32;
            // The separators' places: each spelling swaps some of them.
            let swaps = ask.name.bytes().filter(|it| matches!(it, b'-' | b'_')).count() as u32;
            let spellings = match ask.kind {
                QueryKind::AlternativeNames if swaps >= 10 => MOST_SPELLINGS,
                QueryKind::AlternativeNames => 1 << swaps,
                _ => 1,
            };
            for combo in 0..spellings {
                spelling.clear();
                let mut swap = 0;
                for ch in ask.name.chars() {
                    match ch {
                        '-' | '_' if ask.kind == QueryKind::AlternativeNames => {
                            spelling.push(if combo >> swap & 1 == 1 { '-' } else { '_' });
                            swap += 1;
                        }
                        ch => spelling.push(ch.to_ascii_lowercase()),
                    }
                }
                let file = index.file_of(spelling.as_str()).map_err(refused)?;
                if !index.ask_files.as_slice()[first as usize..].contains(&file) {
                    index.ask_files.push(file);
                }
            }
            let count = index.ask_files.len() as u32 - first;
            index.asks.push((first, count));
        }
        Ok(index)
    }

    /// The file of `spelling` (lower case), made and its read queued the first time.
    fn file_of(&mut self, spelling: &str) -> Result<u32, NameRefused> {
        let mut path = String::new_in(self.alloc);
        request::index_path_in(spelling, &mut path)?;
        if let Some(at) = self.files.iter().position(|file| file.path.as_str() == path.as_str()) {
            return Ok(at as u32);
        }
        let at = self.files.len() as u32;
        self.files.push(File {
            spelling: String::from_str_in(spelling, self.alloc),
            path,
            held: Held::None,
            cached: false,
            phase: Phase::Reading,
            seq: 0,
            end: FileEnd::Pending,
        });
        self.reads.push_back(at);
        self.reads_left += 1;
        self.left += 1;
        Ok(at)
    }

    /// The next effect to do at `now`, or `None` until a completion comes or
    /// [`Index::next_deadline`] passes.
    pub fn poll(&mut self, now: u64) -> Option<IndexEffect<'a>> {
        self.expire(now);
        if let Some(file) = self.reads.pop_front() {
            return Some(IndexEffect::Read { file });
        }
        if let Some((file, record)) = self.writes.pop_front() {
            return Some(IndexEffect::Write { file, record });
        }
        // The requests go as one batch, once every cache record is read.
        if self.reads_left != 0 || self.asked.len() >= self.limits.in_flight_most as usize {
            return None;
        }
        let file = self.ready.pop_front()?;
        Some(self.ask(file, now))
    }

    /// The cache record of `file` as the driver read it: its bytes, or `None` when it has none.
    pub fn read(&mut self, file: u32, record: Option<&[u8]>) {
        let Some(state) = self.files.get(file as usize) else { return };
        if state.phase != Phase::Reading {
            self.stale += 1;
            return;
        }
        // Read: from here the file waits on its request, or has ended.
        self.reads_left -= 1;
        self.files[file as usize].phase = Phase::Ready;
        if let Some(bytes) = record {
            match IndexView::open(bytes) {
                Ok(view) if view.name() != self.files[file as usize].spelling.as_str() => {
                    self.fail(file, IndexWhy::Cache(ViewRefused::OtherFile));
                    return;
                }
                Ok(view) => {
                    let alloc = self.alloc;
                    let held = match view.validator() {
                        Validator::None => Held::None,
                        Validator::Etag(tag) => Held::Etag(String::from_str_in(tag, alloc)),
                        Validator::LastModified(time) => Held::LastModified(String::from_str_in(time, alloc)),
                    };
                    let state = &mut self.files[file as usize];
                    state.held = held;
                    state.cached = true;
                }
                // An old home's record: reported (`rebuilt`) and asked for whole.
                Err(ViewRefused::Old { .. }) => self.rebuilt += 1,
                Err(why) => {
                    self.fail(file, IndexWhy::Cache(why));
                    return;
                }
            }
        }
        if self.files[file as usize].cached && self.update == Update::CacheFirst {
            self.finish(file, FileEnd::Cached);
            return;
        }
        self.ready.push_back(file);
    }

    fn ask(&mut self, file: u32, now: u64) -> IndexEffect<'a> {
        let alloc = self.alloc;
        let state = &self.files[file as usize];
        let mut url = String::with_capacity_in(self.base.len() + state.path.len(), alloc);
        url.str(self.base.as_str()).str(state.path.as_str());
        let condition = match index_planted(IndexPlant::NoValidator) {
            true => None,
            false => request::condition(state.validator()).map(|(name, value)| (name, String::from_str_in(value, alloc))),
        };
        let deadline = now.saturating_add(self.limits.ask_ns);
        let state = &mut self.files[file as usize];
        state.seq += 1;
        state.phase = Phase::Asked { seq: state.seq, deadline };
        let ask = FileAsk { file, seq: state.seq };
        self.asked.push(file);
        IndexEffect::Http { ask, url, headers: &INDEX_HEADERS, condition, most: self.file_limits.most_bytes, deadline_ns: deadline }
    }

    /// The answer to `ask`. No time is taken: nothing waits on an answer (no backoff, D172), and
    /// a deadline is checked at the next [`Index::poll`].
    pub fn answered(&mut self, ask: FileAsk, reply: IndexReply<'_>) {
        let Some(state) = self.files.get(ask.file as usize) else { return };
        let Phase::Asked { seq, .. } = state.phase else {
            self.stale += 1;
            return;
        };
        if seq != ask.seq {
            self.stale += 1;
            return;
        }
        let (file, cached) = (ask.file, state.cached);
        self.asked.retain(|it| *it != file);
        let (status, etag, last_modified, bytes) = match reply {
            // One try (D172): the network's failure is the file's end now.
            IndexReply::Failed(failure) => return self.fail(file, IndexWhy::Net { failure }),
            IndexReply::Body { status, etag, last_modified, bytes } => (status, etag, last_modified, bytes),
        };
        match request::answer_kind(status) {
            AnswerKind::New => {
                let most = self.file_limits.most_bytes;
                if bytes.len() as u64 > most {
                    return self.fail(file, IndexWhy::TooBig { most, got: bytes.len() as u64 });
                }
                self.fetched_bytes += bytes.len() as u64;
                self.parsed += 1;
                let validator = request::validator_of(etag, last_modified);
                let spelling = self.files[file as usize].spelling.as_str();
                match line::parse_in(spelling, bytes, validator, self.file_limits, self.alloc) {
                    Ok(record) => {
                        self.files[file as usize].phase = Phase::Writing;
                        self.writes.push_back((file, record));
                    }
                    Err(why) => self.fail(file, IndexWhy::Refused(why)),
                }
            }
            AnswerKind::NotModified if cached => self.finish(file, FileEnd::Cached),
            AnswerKind::NotModified => self.fail(file, IndexWhy::NotModifiedUnasked),
            AnswerKind::NoSuchCrate => self.finish(file, FileEnd::Absent),
            AnswerKind::Unauthorized => self.fail(file, IndexWhy::Unauthorized),
            // A 429 or 5xx is the registry's word now, as any other status (one try, D172).
            AnswerKind::Transient | AnswerKind::Other => self.fail(file, IndexWhy::Status { status }),
        }
    }

    /// The cache kept `file`'s new record, or did not.
    pub fn written(&mut self, file: u32, kept: Result<(), StoreFailure>) {
        let Some(state) = self.files.get(file as usize) else { return };
        if state.phase != Phase::Writing {
            self.stale += 1;
            return;
        }
        match kept {
            Ok(()) => self.finish(file, FileEnd::Fetched),
            Err(failure) => self.fail(file, IndexWhy::Store(failure)),
        }
    }

    /// The earliest time something is due without a completion: a request's deadline (there is
    /// no backoff: one try, D172).
    pub fn next_deadline(&self) -> Option<u64> {
        self.asked
            .iter()
            .filter_map(|file| match self.files[*file as usize].phase {
                Phase::Asked { deadline, .. } => Some(deadline),
                _ => None,
            })
            .min()
    }

    /// Every file of every ask has ended.
    pub fn is_done(&self) -> bool {
        self.left == 0
    }

    /// The registry, as its cache folder is named.
    pub fn registry(&self) -> RegistryKey {
        self.registry
    }

    /// How many files the batch reads.
    pub fn files(&self) -> u32 {
        self.files.len() as u32
    }

    /// Ask `ask`'s files, in its spellings' order.
    pub fn files_of(&self, ask: u32) -> &[u32] {
        let (first, count) = self.asks[ask as usize];
        &self.ask_files.as_slice()[first as usize..(first + count) as usize]
    }

    /// `file`'s index path (`se/rd/serde`): the URL's tail, and its cache record's under the
    /// registry's folder ([`request::cache_path_in`]).
    pub fn path(&self, file: u32) -> &str {
        self.files[file as usize].path.as_str()
    }

    /// `file`'s name, in lower case.
    pub fn spelling(&self, file: u32) -> &str {
        self.files[file as usize].spelling.as_str()
    }

    pub fn end(&self, file: u32) -> &FileEnd {
        &self.files[file as usize].end
    }

    /// The first of ask `ask`'s files that failed, and why.
    pub fn failure(&self, ask: u32) -> Option<(u32, &IndexWhy)> {
        self.files_of(ask).iter().find_map(|file| match self.end(*file) {
            FileEnd::Failed(why) => Some((*file, why)),
            _ => None,
        })
    }

    /// Bytes of index files downloaded (200 answers' bodies).
    pub fn fetched_bytes(&self) -> u64 {
        self.fetched_bytes
    }

    /// Index files parsed: a 304 or a record that stands parses nothing.
    pub fn parsed(&self) -> u32 {
        self.parsed
    }

    /// Cache records of an old form, asked for whole: the driver reports them.
    pub fn rebuilt(&self) -> u32 {
        self.rebuilt
    }

    /// Answers and completions that came for nothing in flight.
    pub fn stale(&self) -> u64 {
        self.stale
    }

    /// Requests past their deadline failed as timeouts, at once (one try, D172).
    fn expire(&mut self, now: u64) {
        let mut at = 0;
        while at < self.asked.len() {
            let file = self.asked[at];
            match self.files[file as usize].phase {
                Phase::Asked { deadline, .. } if deadline <= now => {
                    self.asked.swap_remove(at);
                    self.fail(file, IndexWhy::Net { failure: NetFailure::Timeout });
                }
                _ => at += 1,
            }
        }
    }

    fn fail(&mut self, file: u32, why: IndexWhy) {
        self.finish(file, FileEnd::Failed(why));
    }

    fn finish(&mut self, file: u32, end: FileEnd) {
        let state = &mut self.files[file as usize];
        state.phase = Phase::Done;
        state.end = end;
        self.left -= 1;
    }
}
