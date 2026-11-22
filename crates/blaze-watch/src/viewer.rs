//! [`Viewer`]: the core of the viewer helper (bottom.md section 14; watch.md). It follows every
//! live run the registry lists (one link a run, over the run's own socket, resumed by sequence
//! number when it drops), merges their streams into one bounded [`Window`], and serves pages:
//! the script, pairing, the list of runs, the merged stream as server-sent events, the profile's
//! history ([`History`]) and the one permitted action, a toggle of hooks forwarded to a run.
//!
//! **Requests are checked in one order**, and a refusal carries no body, so a refused page
//! learns nothing but the status: the `Host` names loopback and the viewer's port (else 421:
//! DNS rebinding); the script is served to anyone (static bytes of the release); the `Origin`
//! is the one origin (else 403, with no CORS headers, so a page of another origin cannot even
//! read the status); a preflight is answered (with Private Network Access); pairing is open to
//! that origin; everything else needs a paired browser's key (else 401).
//!
//! **Memory is fixed when the viewer is made**: the window, at most `most_runs` runs and
//! `most_pages` pages, each page's request at most [`http::MOST_REQUEST`] bytes and its
//! unwritten output at most `page_queue` plus one event. A slow page holds a cursor, not data;
//! when the window drops what it had not read, it gets a gap.
//!
//! **Expiry** (rule 70): with no run listed since `t`, the viewer exits at the first tick at or
//! after `t + idle` when no paired stream is open and none closed in the last `grace`, and at
//! `t + held_most` whatever is open. watch.md says why these numbers.

// adapted from makepad/relax (MIT), libs/remote/src/hub.rs (a helper started on demand that
// follows feeds of many endpoints, reopens them from the last sequence number, shows a gap
// marker, and exits a fixed time after its last client; its socket in the user's own 0700
// directory); changed: runs found from the registry, pages over HTTP and SSE with an origin
// check and pairing, the expiry bounded even while a page is open.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{Vec, soa};

use crate::cbor::{self, Reader};
use crate::conn::{ConnError, Grant};
use crate::feed::{Follow, FollowError, Item};
use crate::frame::{self, Frame, Role, Service};
use crate::http::{self, Body, Cors, Method, Parsed, Request, Status};
use crate::pairing::{Code, Key, PairError, Pairing, Poll, Ticket};
use crate::run_wire::{self, Entry, RunName};
use crate::window::{Window, WindowError};
use crate::{Nanos, Plant, Random, Seq};

/// The viewer's fixed loopback port: any blaze command that finds nothing listening there
/// starts the viewer.
pub const DEFAULT_PORT: u16 = 47_823;
/// The one origin let in.
pub const ORIGIN: &str = "https://ix.dev";
/// The most bytes of one item in the window.
pub const MOST_ITEM: usize = 256;
/// A page's request must be whole this long after its connection opened.
pub const REQUEST_FOR: Nanos = Nanos::secs(10);
/// The most rows one history answer has.
pub const MOST_HISTORY: usize = 256;
/// The page's script path.
pub const SCRIPT: &str = "/v1/app.js";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    pub origin: &'static str,
    pub port: u16,
    /// The viewer's own user: a local command of another user is refused.
    pub uid: u32,
    /// The page's script, as the blaze binary carries it.
    pub script: &'static [u8],
    pub idle: Nanos,
    pub held_most: Nanos,
    pub grace: Nanos,
    pub keepalive: Nanos,
    /// The most bytes given to a page's connection and not yet written.
    pub page_queue: usize,
    pub window_bytes: usize,
    pub window_items: usize,
    pub most_pages: usize,
    pub most_runs: usize,
    /// The most slots one toggle names.
    pub most_slots: usize,
}

impl Config {
    pub const fn new(uid: u32, script: &'static [u8]) -> Config {
        Config {
            origin: ORIGIN,
            port: DEFAULT_PORT,
            uid,
            script,
            idle: Nanos::secs(30),
            held_most: Nanos::secs(600),
            grace: Nanos::secs(5),
            keepalive: Nanos::secs(15),
            page_queue: 64 << 10,
            window_bytes: 4 << 20,
            window_items: 1 << 16,
            most_pages: 16,
            most_runs: 256,
            most_slots: 4096,
        }
    }
}

/// A page's connection, as the driver numbers it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PageId(pub u64);

/// A local command's connection to the viewer's Unix socket.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LocalId(pub u64);

/// What a local blaze command asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask {
    /// Is a browser waiting to pair? (Every command asks, so the next one shows the code.)
    Pending,
    /// The person said the browser shows `code`.
    Confirm(Code),
    Decline,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event<'a> {
    /// The driver's timer, about once a second.
    Tick { now: Nanos },
    /// The registry as just read (blaze-run's `Registry::read`, which never lists a dead run),
    /// sorted by name.
    Listed { now: Nanos, live: &'a [Entry] },
    /// A frame from a run's socket.
    RunFrame { run: RunName, frame: Frame<'a>, now: Nanos },
    /// A run's socket closed or could not be opened.
    RunClosed { run: RunName },
    PageOpened { page: PageId, now: Nanos },
    PageBytes { page: PageId, bytes: &'a [u8], now: Nanos },
    /// The driver wrote `bytes` of what was given to the page.
    PageWrote { page: PageId, bytes: usize, now: Nanos },
    PageClosed { page: PageId, now: Nanos },
    /// A local command, whose peer credentials say `uid`.
    Local { conn: LocalId, uid: u32, ask: Ask, now: Nanos },
}

/// Bytes of [`Out::bytes`] an effect carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub at: usize,
    pub len: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Open the run's socket (`<name>.sock` beside its registry entry); the bytes for it follow.
    Connect { run: RunName },
    ToRun { run: RunName, span: Span },
    /// A REQUEST frame with a toggle, for the run's parent.
    Toggle { run: RunName, span: Span },
    /// Close the run's socket: it is gone from the registry, or broke the protocol (`why`).
    Unlink { run: RunName, why: Option<LinkError> },
    ToPage { page: PageId, span: Span },
    /// Close the page's connection once what was given to it is written.
    ClosePage { page: PageId },
    ToLocal { conn: LocalId, span: Span },
    /// Write the keys file whole (mode 0600, by rename).
    StoreKeys { span: Span },
    Exit,
}

/// Where a step's effects go: the effects, and the bytes they carry.
pub struct Out<A: Allocator> {
    pub effects: Vec<Effect, A>,
    pub bytes: Vec<u8, A>,
}

impl<A: Allocator + Clone> Out<A> {
    pub fn new_in(alloc: A) -> Out<A> {
        Out { effects: Vec::new_in(alloc.clone()), bytes: Vec::new_in(alloc) }
    }
}

impl<A: Allocator> Out<A> {
    pub fn get(&self, span: Span) -> &[u8] {
        &self.bytes[span.at..span.at + span.len]
    }

    pub fn clear(&mut self) {
        self.effects.clear();
        self.bytes.clear();
    }

    fn write(&mut self, f: impl FnOnce(&mut Vec<u8, A>)) -> Span {
        let at = self.bytes.len();
        f(&mut self.bytes);
        Span { at, len: self.bytes.len() - at }
    }
}

/// Why a run's link was cut.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkError {
    Conn(ConnError),
    Follow(FollowError),
    Item(cbor::Error),
    /// A frame a run does not send the viewer (a request, a frame on an unopened channel).
    Unexpected { kind: &'static str },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Conn(error) => write!(f, "{error}"),
            LinkError::Follow(error) => write!(f, "{error}"),
            LinkError::Item(error) => write!(f, "an item: {error}"),
            LinkError::Unexpected { kind } => write!(f, "a run sent the viewer a {kind}"),
        }
    }
}

/// The driver's own mistakes: an event about a page it never opened, more written than given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewerError {
    UnknownPage { page: PageId },
    Wrote { page: PageId, wrote: usize, queued: usize },
    Window(WindowError),
    Pairing(PairError),
    /// An event after the viewer said `Exit`.
    Exited,
}

impl fmt::Display for ViewerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ViewerError::UnknownPage { page } => write!(f, "an event of page connection {}, never opened", page.0),
            ViewerError::Wrote { page, wrote, queued } => {
                write!(f, "page connection {} wrote {wrote} bytes of {queued} given", page.0)
            }
            ViewerError::Window(error) => write!(f, "the viewer's window: {error}"),
            ViewerError::Pairing(error) => write!(f, "the viewer's pairing: {error}"),
            ViewerError::Exited => write!(f, "an event for a viewer that has exited"),
        }
    }
}

impl core::error::Error for ViewerError {}

impl From<WindowError> for ViewerError {
    fn from(error: WindowError) -> ViewerError {
        ViewerError::Window(error)
    }
}

/// What the profile's history is asked (POST `/v1/history`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Query {
    pub since: u64,
    pub until: u64,
    pub limit: usize,
}

/// One ended run, as the profile keeps it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Past {
    pub name: RunName,
    pub checkout: [u8; 32],
    pub program: [u8; 32],
    pub began: u64,
    pub ended: u64,
    pub status: i32,
    pub peak: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HistoryError {
    /// The profile could not be read now (named by the driver's log, not the page).
    Unavailable,
}

/// The profile's history of runs. The profile crate implements it; a query is bounded by its
/// limit, at most [`MOST_HISTORY`] rows, pushed newest first.
pub trait History {
    fn query<A: Allocator>(&mut self, query: &Query, out: &mut Vec<Past, A>) -> Result<(), HistoryError>;
}

/// The state of the link to one run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    /// Asked to connect; the run's HELLO has not come.
    Connecting,
    Open,
    /// Closed or cut: connected again at the next listing that still lists the run.
    Lost,
}

soa! {
    /// One live run the viewer follows.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Run, Runs, RunRef {
        pub name: RunName => names / names_mut,
        pub checkout: [u8; 32] => checkouts / checkouts_mut,
        pub program: [u8; 32] => programs / programs_mut,
        pub began: u64 => begans / begans_mut,
        pub link: Link => links / links_mut,
        pub follow: Follow => follows / follows_mut,
        pub grant: Grant => grants / grants_mut,
    }
}

/// One item of the merged stream, as pages get it (base64 of its CBOR in an event's data).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageItem {
    /// `[1, 0, pid, start, seq, item]`: item `seq` of the run's stream.
    Run { name: RunName, seq: Seq, item: Item },
    /// `[1, 1, pid, start, checkout, program, began]`: the registry lists a new run.
    Listed { name: RunName, checkout: [u8; 32], program: [u8; 32], began: u64 },
    /// `[1, 2, pid, start]`: the run is gone from the registry.
    Gone { name: RunName },
    /// `[1, 3, pid, start, from, to]`: the run's items `from..=to` did not reach the viewer.
    Gap { name: RunName, from: Seq, to: Seq },
}

const PAGE_VERSION: u64 = 1;

impl PageItem {
    pub fn write<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        let head = |out: &mut Vec<u8, A>, len: u64, kind: u64, name: RunName| {
            cbor::array(out, len);
            cbor::uint(out, PAGE_VERSION);
            cbor::uint(out, kind);
            cbor::uint(out, u64::from(name.pid));
            cbor::uint(out, name.start);
        };
        match *self {
            PageItem::Run { name, seq, item } => {
                head(out, 6, 0, name);
                cbor::uint(out, seq.0);
                item.write(out);
            }
            PageItem::Listed { name, checkout, program, began } => {
                head(out, 7, 1, name);
                cbor::bytes(out, &checkout);
                cbor::bytes(out, &program);
                cbor::uint(out, began);
            }
            PageItem::Gone { name } => head(out, 4, 2, name),
            PageItem::Gap { name, from, to } => {
                head(out, 6, 3, name);
                cbor::uint(out, from.0);
                cbor::uint(out, to.0);
            }
        }
    }

    pub fn read(bytes: &[u8]) -> Result<PageItem, cbor::Error> {
        let mut r = Reader::new(bytes);
        let fields = r.array(7, "a page item of more fields than any kind has")?;
        if r.uint()? != PAGE_VERSION {
            return Err(cbor::Error { at: 1, why: "a page item of another schema" });
        }
        let kind = r.uint()?;
        let pid = r.uint_most(u64::from(u32::MAX), "a pid past 32 bits")? as u32;
        let name = RunName { pid, start: r.uint()? };
        let item = match (kind, fields) {
            (0, 6) => {
                let seq = Seq(r.uint()?);
                let at = r.at();
                let item = Item::read(&bytes[at..])?;
                return Ok(PageItem::Run { name, seq, item });
            }
            (1, 7) => PageItem::Listed { name, checkout: r.fixed()?, program: r.fixed()?, began: r.uint()? },
            (2, 4) => PageItem::Gone { name },
            (3, 6) => PageItem::Gap { name, from: Seq(r.uint()?), to: Seq(r.uint()?) },
            _ => return Err(cbor::Error { at: 0, why: "a page item of unknown kind or length" }),
        };
        r.end()?;
        Ok(item)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PageState {
    /// Its request is not whole yet.
    Reading,
    /// An event stream: sent up to `cursor`.
    Streaming,
    /// Answered; closes once written.
    Closing,
}

struct Page<A: Allocator> {
    id: PageId,
    inbox: Vec<u8, A>,
    state: PageState,
    cursor: Seq,
    queued: usize,
    opened: Nanos,
    last_sent: Nanos,
}

pub struct Viewer<A: Allocator + Clone> {
    config: Config,
    epoch: u64,
    runs: Runs<A>,
    // Rows of structs: every access reads the whole row, and there are at most `most_pages`.
    pages: Vec<Page<A>, A>,
    window: Window<A>,
    pairing: Pairing<A>,
    scratch: Vec<u8, A>,
    request: Vec<u8, A>,
    slots: Vec<u32, A>,
    pasts: Vec<Past, A>,
    alloc: A,
    empty_since: Option<Nanos>,
    last_stream_close: Nanos,
    exited: bool,
    pub(crate) plant: Plant,
}

impl<A: Allocator + Clone> Viewer<A> {
    /// A viewer started at `now`, with the browser keys its file holds.
    pub fn new_in<R: Random>(
        config: Config,
        keys: &[Key],
        now: Nanos,
        random: &mut R,
        alloc: A,
    ) -> Result<Viewer<A>, ViewerError> {
        let mut epoch = [0u8; 8];
        random.fill(&mut epoch);
        Ok(Viewer {
            config,
            epoch: u64::from_le_bytes(epoch) >> 1,
            runs: Runs::new_in(alloc.clone()),
            pages: Vec::with_capacity_in(config.most_pages, alloc.clone()),
            window: Window::new_in(config.window_bytes, config.window_items, alloc.clone())?,
            pairing: Pairing::new_in(keys, alloc.clone()).map_err(ViewerError::Pairing)?,
            scratch: Vec::with_capacity_in(MOST_ITEM, alloc.clone()),
            request: Vec::with_capacity_in(64, alloc.clone()),
            slots: Vec::with_capacity_in(config.most_slots, alloc.clone()),
            pasts: Vec::with_capacity_in(MOST_HISTORY, alloc.clone()),
            alloc,
            empty_since: Some(now),
            last_stream_close: now,
            exited: false,
            plant: Plant::None,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn window(&self) -> &Window<A> {
        &self.window
    }

    pub fn runs(&self) -> &Runs<A> {
        &self.runs
    }

    /// Bytes given to `page` and not yet written.
    pub fn queued(&self, page: PageId) -> Option<usize> {
        self.pages.iter().find(|p| p.id == page).map(|p| p.queued)
    }

    pub fn streams(&self) -> usize {
        self.pages.iter().filter(|p| p.state == PageState::Streaming).count()
    }

    pub fn step<R: Random, H: History, B: Allocator>(
        &mut self,
        event: Event<'_>,
        random: &mut R,
        history: &mut H,
        out: &mut Out<B>,
    ) -> Result<(), ViewerError> {
        if self.exited {
            return Err(ViewerError::Exited);
        }
        match event {
            Event::Tick { now } => self.tick(now, out),
            Event::Listed { now, live } => self.listed(now, live, out),
            Event::RunFrame { run, frame, now } => self.run_frame(run, frame, now, out),
            Event::RunClosed { run } => {
                if let Some(row) = self.row_of(run) {
                    self.runs.links_mut()[row] = Link::Lost;
                }
                Ok(())
            }
            Event::PageOpened { page, now } => self.page_opened(page, now, out),
            Event::PageBytes { page, bytes, now } => self.page_bytes(page, bytes, now, random, history, out),
            Event::PageWrote { page, bytes, now } => {
                let at = self.page_at(page)?;
                let queued = self.pages[at].queued;
                if bytes > queued {
                    return Err(ViewerError::Wrote { page, wrote: bytes, queued });
                }
                self.pages[at].queued = queued - bytes;
                self.pump(at, now, out)
            }
            Event::PageClosed { page, now } => {
                let at = self.page_at(page)?;
                if self.pages[at].state == PageState::Streaming {
                    self.last_stream_close = now;
                }
                self.pages.swap_remove(at);
                Ok(())
            }
            Event::Local { conn, uid, ask, now } => self.local(conn, uid, ask, now, random, out),
        }
    }

    fn row_of(&self, run: RunName) -> Option<usize> {
        self.runs.names().iter().position(|name| *name == run)
    }

    fn page_at(&self, page: PageId) -> Result<usize, ViewerError> {
        self.pages.iter().position(|p| p.id == page).ok_or(ViewerError::UnknownPage { page })
    }

    // ---------------------------------------------------------------- expiry

    /// Whether the expiry rule says exit at `now`.
    pub fn due(&self, now: Nanos) -> bool {
        let Some(since) = self.empty_since else {
            return false;
        };
        if !self.runs.is_empty() {
            return false;
        }
        if self.plant != Plant::PageHoldsForever && now >= since.after(self.config.held_most) {
            return true;
        }
        self.streams() == 0
            && now >= since.after(self.config.idle)
            && now >= self.last_stream_close.after(self.config.grace)
    }

    fn tick<B: Allocator>(&mut self, now: Nanos, out: &mut Out<B>) -> Result<(), ViewerError> {
        if self.due(now) {
            self.exited = true;
            out.effects.push(Effect::Exit);
            return Ok(());
        }
        for at in 0..self.pages.len() {
            let page = &mut self.pages[at];
            match page.state {
                PageState::Reading if now.since(page.opened) > REQUEST_FOR => {
                    page.state = PageState::Closing;
                    out.effects.push(Effect::ClosePage { page: page.id });
                }
                PageState::Streaming if now.since(page.last_sent) >= self.config.keepalive => {
                    let span = out.write(http::sse_comment);
                    page.queued += span.len;
                    page.last_sent = now;
                    out.effects.push(Effect::ToPage { page: page.id, span });
                }
                _ => {}
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- runs

    fn push_item(&mut self, item: &PageItem) -> Result<(), ViewerError> {
        self.scratch.clear();
        item.write(&mut self.scratch);
        self.window.push(&self.scratch)?;
        Ok(())
    }

    fn pump_all<B: Allocator>(&mut self, now: Nanos, out: &mut Out<B>) -> Result<(), ViewerError> {
        for at in 0..self.pages.len() {
            self.pump(at, now, out)?;
        }
        Ok(())
    }

    fn hello_run<B: Allocator>(&mut self, run: RunName, out: &mut Out<B>) {
        out.effects.push(Effect::Connect { run });
        let epoch = self.epoch;
        let span = out.write(|bytes| Frame::Hello { role: Role::Viewer, epoch }.write(bytes));
        out.effects.push(Effect::ToRun { run, span });
    }

    fn listed<B: Allocator>(&mut self, now: Nanos, live: &[Entry], out: &mut Out<B>) -> Result<(), ViewerError> {
        let live = &live[..live.len().min(self.config.most_runs)];
        let mut row = 0;
        while row < self.runs.len() {
            let name = self.runs.names()[row];
            if live.iter().any(|entry| entry.name == name) {
                row += 1;
                continue;
            }
            self.runs.swap_remove(row);
            out.effects.push(Effect::Unlink { run: name, why: None });
            self.push_item(&PageItem::Gone { name })?;
        }
        for entry in live {
            match self.row_of(entry.name) {
                None => {
                    self.runs.push(Run {
                        name: entry.name,
                        checkout: entry.checkout,
                        program: entry.program,
                        began: entry.began,
                        link: Link::Connecting,
                        follow: Follow::default(),
                        grant: Grant::default(),
                    });
                    self.hello_run(entry.name, out);
                    self.push_item(&PageItem::Listed {
                        name: entry.name,
                        checkout: entry.checkout,
                        program: entry.program,
                        began: entry.began,
                    })?;
                }
                Some(row) if self.runs.links()[row] == Link::Lost => {
                    self.runs.links_mut()[row] = Link::Connecting;
                    self.hello_run(entry.name, out);
                }
                Some(_) => {}
            }
        }
        match (self.runs.is_empty(), self.empty_since) {
            (true, None) => self.empty_since = Some(now),
            (false, Some(_)) => self.empty_since = None,
            _ => {}
        }
        self.pump_all(now, out)
    }

    fn cut<B: Allocator>(&mut self, row: usize, why: LinkError, out: &mut Out<B>) {
        self.runs.links_mut()[row] = Link::Lost;
        out.effects.push(Effect::Unlink { run: self.runs.names()[row], why: Some(why) });
    }

    fn run_frame<B: Allocator>(
        &mut self,
        run: RunName,
        frame: Frame<'_>,
        now: Nanos,
        out: &mut Out<B>,
    ) -> Result<(), ViewerError> {
        // A frame of a run no longer listed was already unlinked.
        let Some(row) = self.row_of(run) else {
            return Ok(());
        };
        if self.runs.links()[row] == Link::Lost {
            return Ok(());
        }
        match frame {
            Frame::Hello { role: Role::Run, epoch } => {
                let from = self.runs.follows_mut()[row].hello(epoch);
                self.runs.links_mut()[row] = Link::Open;
                self.runs.grants_mut()[row] = Grant::default();
                let span = out.write(|bytes| {
                    Frame::Open { chan: frame::EVENTS, service: Service::Events, from }.write(bytes)
                });
                out.effects.push(Effect::ToRun { run, span });
            }
            Frame::Data { chan: frame::EVENTS, seq, bytes } if self.runs.links()[row] == Link::Open => {
                let len = Frame::data_len(frame::EVENTS, seq, bytes.len());
                if let Err(error) = self.runs.grants_mut()[row].received(frame::EVENTS, len) {
                    self.cut(row, LinkError::Conn(error), out);
                    return Ok(());
                }
                if let Err(error) = self.runs.follows_mut()[row].data(seq) {
                    self.cut(row, LinkError::Follow(error), out);
                    return Ok(());
                }
                let item = match Item::read(bytes) {
                    Ok(item) => item,
                    Err(error) => {
                        self.cut(row, LinkError::Item(error), out);
                        return Ok(());
                    }
                };
                self.push_item(&PageItem::Run { name: run, seq, item })?;
                if let Some(grant) = self.runs.grants_mut()[row].consumed(len) {
                    let span = out.write(|b| Frame::Credit { chan: frame::EVENTS, bytes: grant }.write(b));
                    out.effects.push(Effect::ToRun { run, span });
                }
                self.pump_all(now, out)?;
            }
            Frame::Gap { chan: frame::EVENTS, from, to } if self.runs.links()[row] == Link::Open => {
                if let Err(error) = self.runs.follows_mut()[row].gap(from, to) {
                    self.cut(row, LinkError::Follow(error), out);
                    return Ok(());
                }
                self.push_item(&PageItem::Gap { name: run, from, to })?;
                self.pump_all(now, out)?;
            }
            Frame::Ping { at } => {
                let span = out.write(|b| Frame::Pong { at }.write(b));
                out.effects.push(Effect::ToRun { run, span });
            }
            Frame::Pong { .. } => {}
            Frame::Close { .. } => self.runs.links_mut()[row] = Link::Lost,
            Frame::Hello { .. } => self.cut(row, LinkError::Unexpected { kind: "hello of another role" }, out),
            Frame::Request { .. } => self.cut(row, LinkError::Unexpected { kind: "request" }, out),
            Frame::Open { .. } | Frame::Credit { .. } | Frame::Data { .. } | Frame::Gap { .. } => {
                self.cut(row, LinkError::Unexpected { kind: "frame on a channel not open" }, out)
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- pages

    fn page_opened<B: Allocator>(&mut self, page: PageId, now: Nanos, out: &mut Out<B>) -> Result<(), ViewerError> {
        if self.pages.len() >= self.config.most_pages {
            let span = out.write(|b| http::respond(b, Status::Unavailable, Cors::NONE, Body::Empty));
            out.effects.push(Effect::ToPage { page, span });
            out.effects.push(Effect::ClosePage { page });
            return Ok(());
        }
        self.pages.push(Page {
            id: page,
            inbox: Vec::new_in(self.alloc.clone()),
            state: PageState::Reading,
            cursor: Seq::NONE,
            queued: 0,
            opened: now,
            last_sent: now,
        });
        Ok(())
    }

    /// Gives `page` a whole answer and closes it.
    fn answer<B: Allocator>(&mut self, at: usize, status: Status, cors: Cors<'_>, body: Body<'_>, out: &mut Out<B>) {
        let span = out.write(|b| http::respond(b, status, cors, body));
        let page = &mut self.pages[at];
        page.queued += span.len;
        page.state = PageState::Closing;
        out.effects.push(Effect::ToPage { page: page.id, span });
        out.effects.push(Effect::ClosePage { page: page.id });
    }

    fn page_bytes<R: Random, H: History, B: Allocator>(
        &mut self,
        page: PageId,
        bytes: &[u8],
        now: Nanos,
        random: &mut R,
        history: &mut H,
        out: &mut Out<B>,
    ) -> Result<(), ViewerError> {
        let at = self.page_at(page)?;
        if self.pages[at].state != PageState::Reading {
            // Bytes after the request: one request a connection.
            return Ok(());
        }
        if self.pages[at].inbox.len() + bytes.len() > http::MOST_REQUEST {
            self.answer(at, Status::HeadersTooLarge, Cors::NONE, Body::Empty, out);
            return Ok(());
        }
        self.pages[at].inbox.extend_from_slice(bytes);
        let inbox = core::mem::replace(&mut self.pages[at].inbox, Vec::new_in(self.alloc.clone()));
        match http::parse(&inbox) {
            Ok(Parsed::More) => {
                self.pages[at].inbox = inbox;
                Ok(())
            }
            Err(status) => {
                self.answer(at, status, Cors::NONE, Body::Empty, out);
                Ok(())
            }
            Ok(Parsed::Done(request)) => self.route(at, &request, now, random, history, out),
        }
    }

    fn authorized(&self, request: &Request<'_>) -> bool {
        request
            .authorization
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(http::key_from_base64url)
            .is_some_and(|key| self.pairing.check(&key))
    }

    fn route<R: Random, H: History, B: Allocator>(
        &mut self,
        at: usize,
        request: &Request<'_>,
        now: Nanos,
        random: &mut R,
        history: &mut H,
        out: &mut Out<B>,
    ) -> Result<(), ViewerError> {
        let config = self.config;
        if !request.host.is_some_and(|host| http::loopback_host(host, config.port)) {
            self.answer(at, Status::Misdirected, Cors::NONE, Body::Empty, out);
            return Ok(());
        }
        if request.method == Method::Get && request.path == SCRIPT {
            self.answer(at, Status::Ok, Cors::NONE, Body::Script(config.script), out);
            return Ok(());
        }
        let cors = Cors { origin: Some(config.origin), private_network: request.private_network, preflight: false };
        if self.plant == Plant::RouteBeforeOrigin && request.path == "/v1/runs" {
            return self.answer_runs(at, cors, out);
        }
        if request.origin != Some(config.origin) {
            self.answer(at, Status::Forbidden, Cors::NONE, Body::Empty, out);
            return Ok(());
        }
        if request.method == Method::Options {
            let status = match request.preflight.is_some() {
                true => Status::NoContent,
                false => Status::MethodNotAllowed,
            };
            self.answer(at, status, Cors { preflight: true, ..cors }, Body::Empty, out);
            return Ok(());
        }
        let cors = Cors { private_network: false, ..cors };
        match (request.method, request.path) {
            (Method::Post, "/v1/pair") => {
                let (ticket, code) = self.pairing.start(now, random);
                self.request.clear();
                cbor::array(&mut self.request, 3);
                cbor::uint(&mut self.request, PAGE_VERSION);
                cbor::bytes(&mut self.request, &ticket.0);
                cbor::text(&mut self.request, code.as_str());
                let body = core::mem::replace(&mut self.request, Vec::new_in(self.alloc.clone()));
                self.answer(at, Status::Ok, cors, Body::Cbor(&body), out);
                self.request = body;
                return Ok(());
            }
            (Method::Get, path) if path.starts_with("/v1/pair/") => {
                let Some(ticket) = http::hex::<16>(&path["/v1/pair/".len()..]) else {
                    self.answer(at, Status::NotFound, cors, Body::Empty, out);
                    return Ok(());
                };
                self.request.clear();
                let found = match self.pairing.poll(Ticket(ticket), now) {
                    Poll::Unknown => false,
                    Poll::Waiting => {
                        cbor::array(&mut self.request, 2);
                        cbor::uint(&mut self.request, PAGE_VERSION);
                        cbor::uint(&mut self.request, 0);
                        true
                    }
                    Poll::Key(key) => {
                        cbor::array(&mut self.request, 3);
                        cbor::uint(&mut self.request, PAGE_VERSION);
                        cbor::uint(&mut self.request, 1);
                        cbor::bytes(&mut self.request, &key.0);
                        true
                    }
                };
                let body = core::mem::replace(&mut self.request, Vec::new_in(self.alloc.clone()));
                match found {
                    true => self.answer(at, Status::Ok, cors, Body::Cbor(&body), out),
                    false => self.answer(at, Status::NotFound, cors, Body::Empty, out),
                }
                self.request = body;
                return Ok(());
            }
            (Method::Post, "/v1/toggle") if self.plant == Plant::ToggleBeforeKey => {
                return self.toggle(at, request.body, cors, out);
            }
            _ => {}
        }
        if !self.authorized(request) {
            self.answer(at, Status::Unauthorized, cors, Body::Empty, out);
            return Ok(());
        }
        match (request.method, request.path) {
            (Method::Get, "/v1/runs") => self.answer_runs(at, cors, out),
            (Method::Get, "/v1/events") => self.stream(at, request, now, cors, out),
            (Method::Post, "/v1/history") => self.history(at, request.body, cors, history, out),
            (Method::Post, "/v1/toggle") => self.toggle(at, request.body, cors, out),
            _ => {
                self.answer(at, Status::NotFound, cors, Body::Empty, out);
                Ok(())
            }
        }
    }

    /// `[1, [[pid, start, checkout, program, began, open], ...]]`.
    fn answer_runs<B: Allocator>(&mut self, at: usize, cors: Cors<'_>, out: &mut Out<B>) -> Result<(), ViewerError> {
        self.request.clear();
        let body = &mut self.request;
        cbor::array(body, 2);
        cbor::uint(body, PAGE_VERSION);
        cbor::array(body, self.runs.len() as u64);
        for run in self.runs.iter() {
            cbor::array(body, 6);
            cbor::uint(body, u64::from(run.name.pid));
            cbor::uint(body, run.name.start);
            cbor::bytes(body, run.checkout);
            cbor::bytes(body, run.program);
            cbor::uint(body, *run.began);
            cbor::boolean(body, *run.link == Link::Open);
        }
        let body = core::mem::replace(&mut self.request, Vec::new_in(self.alloc.clone()));
        self.answer(at, Status::Ok, cors, Body::Cbor(&body), out);
        self.request = body;
        Ok(())
    }

    fn stream<B: Allocator>(
        &mut self,
        at: usize,
        request: &Request<'_>,
        now: Nanos,
        cors: Cors<'_>,
        out: &mut Out<B>,
    ) -> Result<(), ViewerError> {
        let start = Seq(self.window.first().0 - 1);
        let cursor = match request.last_event_id.map(http::event_id) {
            None => start,
            Some(None) => {
                self.answer(at, Status::BadRequest, cors, Body::Empty, out);
                return Ok(());
            }
            // Numbers of another viewer process: start again.
            Some(Some((epoch, _))) if epoch != self.epoch => start,
            Some(Some((_, seq))) if seq > self.window.newest().0 => {
                self.answer(at, Status::BadRequest, cors, Body::Empty, out);
                return Ok(());
            }
            Some(Some((_, seq))) => Seq(seq),
        };
        let origin = self.config.origin;
        let epoch = self.epoch;
        let (first, newest) = (self.window.first(), self.window.newest());
        let span = out.write(|b| {
            http::sse_head(b, origin);
            // The stream's first event, with no id: whose numbers follow, and what is held.
            b.extend_from_slice(b"event: hello\ndata: ");
            let mut hello = [0u8; 40];
            let len = {
                let mut w = Cursor { buf: &mut hello, at: 0 };
                w.array(4);
                w.uint(PAGE_VERSION);
                w.uint(epoch);
                w.uint(first.0);
                w.uint(newest.0);
                w.at
            };
            http::base64(b, &hello[..len]);
            b.extend_from_slice(b"\n\n");
        });
        let page = &mut self.pages[at];
        page.state = PageState::Streaming;
        page.cursor = cursor;
        page.queued += span.len;
        page.last_sent = now;
        out.effects.push(Effect::ToPage { page: page.id, span });
        self.pump(at, now, out)
    }

    /// Sends the stream page `at` has room for: a gap for what the window dropped, then items.
    fn pump<B: Allocator>(&mut self, at: usize, now: Nanos, out: &mut Out<B>) -> Result<(), ViewerError> {
        if self.pages[at].state != PageState::Streaming {
            return Ok(());
        }
        let start = out.bytes.len();
        let epoch = self.epoch;
        loop {
            let page = &mut self.pages[at];
            if self.plant != Plant::IgnorePageQueue && page.queued + (out.bytes.len() - start) >= self.config.page_queue {
                break;
            }
            let want = page.cursor.next();
            if want >= self.window.next() {
                break;
            }
            if want < self.window.first() {
                let to = Seq(self.window.first().0 - 1);
                let mut gap = [0u8; 32];
                let len = {
                    let mut w = Cursor { buf: &mut gap, at: 0 };
                    w.array(3);
                    w.uint(PAGE_VERSION);
                    w.uint(want.0);
                    w.uint(to.0);
                    w.at
                };
                http::sse_event(&mut out.bytes, epoch, to.0, "gap", &gap[..len]);
                page.cursor = to;
                continue;
            }
            self.scratch.clear();
            self.window.copy_into(want, &mut self.scratch)?;
            http::sse_event(&mut out.bytes, epoch, want.0, "e", &self.scratch);
            self.pages[at].cursor = want;
        }
        let len = out.bytes.len() - start;
        if len > 0 {
            let page = &mut self.pages[at];
            page.queued += len;
            page.last_sent = now;
            out.effects.push(Effect::ToPage { page: page.id, span: Span { at: start, len } });
        }
        Ok(())
    }

    /// POST `/v1/history`: `[1, since, until, limit]` -> `[1, [[pid, start, checkout, program,
    /// began, ended, status, peak], ...]]` (the status as its 32 bits, unsigned).
    fn history<H: History, B: Allocator>(
        &mut self,
        at: usize,
        body: &[u8],
        cors: Cors<'_>,
        history: &mut H,
        out: &mut Out<B>,
    ) -> Result<(), ViewerError> {
        let query = (|| {
            let mut r = Reader::new(body);
            r.array_of(4)?;
            if r.uint()? != PAGE_VERSION {
                return Err(cbor::Error { at: 1, why: "a query of another schema" });
            }
            let since = r.uint()?;
            let until = r.uint()?;
            let limit = r.uint_most(MOST_HISTORY as u64, "a limit past the most rows")? as usize;
            r.end()?;
            Ok(Query { since, until, limit })
        })();
        let Ok(query) = query else {
            self.answer(at, Status::BadRequest, cors, Body::Empty, out);
            return Ok(());
        };
        self.pasts.clear();
        if history.query(&query, &mut self.pasts).is_err() {
            self.answer(at, Status::Unavailable, cors, Body::Empty, out);
            return Ok(());
        }
        self.pasts.truncate(query.limit);
        self.request.clear();
        let b = &mut self.request;
        cbor::array(b, 2);
        cbor::uint(b, PAGE_VERSION);
        cbor::array(b, self.pasts.len() as u64);
        for past in self.pasts.iter() {
            cbor::array(b, 8);
            cbor::uint(b, u64::from(past.name.pid));
            cbor::uint(b, past.name.start);
            cbor::bytes(b, &past.checkout);
            cbor::bytes(b, &past.program);
            cbor::uint(b, past.began);
            cbor::uint(b, past.ended);
            cbor::uint(b, u64::from(past.status as u32));
            cbor::uint(b, past.peak);
        }
        let body = core::mem::replace(&mut self.request, Vec::new_in(self.alloc.clone()));
        self.answer(at, Status::Ok, cors, Body::Cbor(&body), out);
        self.request = body;
        Ok(())
    }

    /// POST `/v1/toggle`: `[1, pid, start, hooks, on, [slot, ...]]`, forwarded to the run as
    /// blaze-run's `Toggle`. 202 once given to the run's link; 404 for a run not listed; 503
    /// for one whose link is not open.
    fn toggle<B: Allocator>(&mut self, at: usize, body: &[u8], cors: Cors<'_>, out: &mut Out<B>) -> Result<(), ViewerError> {
        self.slots.clear();
        let most = self.config.most_slots;
        let slots = &mut self.slots;
        let parsed = (|| {
            let mut r = Reader::new(body);
            r.array_of(6)?;
            if r.uint()? != PAGE_VERSION {
                return Err(cbor::Error { at: 1, why: "a toggle of another schema" });
            }
            let pid = r.uint_most(u64::from(u32::MAX), "a pid past 32 bits")? as u32;
            let start = r.uint()?;
            let hooks = r.uint_most(u64::from(u16::MAX), "hooks past 16 bits")? as u16;
            let on = r.boolean()?;
            let len = r.array(most, "a toggle of more slots than the cap")?;
            for _ in 0..len {
                slots.push(r.uint_most(u64::from(u32::MAX), "a slot past 32 bits")? as u32);
            }
            r.end()?;
            Ok((RunName { pid, start }, hooks, on))
        })();
        let Ok((run, hooks, on)) = parsed else {
            self.answer(at, Status::BadRequest, cors, Body::Empty, out);
            return Ok(());
        };
        let Some(row) = self.row_of(run) else {
            self.answer(at, Status::NotFound, cors, Body::Empty, out);
            return Ok(());
        };
        if self.runs.links()[row] != Link::Open {
            self.answer(at, Status::Unavailable, cors, Body::Empty, out);
            return Ok(());
        }
        self.request.clear();
        run_wire::toggle(&self.slots, hooks, on, &mut self.request);
        let request = &self.request;
        let span = out.write(|b| Frame::Request { bytes: request }.write(b));
        out.effects.push(Effect::Toggle { run, span });
        self.request.clear();
        cbor::array(&mut self.request, 1);
        cbor::uint(&mut self.request, PAGE_VERSION);
        let body = core::mem::replace(&mut self.request, Vec::new_in(self.alloc.clone()));
        self.answer(at, Status::Accepted, cors, Body::Cbor(&body), out);
        self.request = body;
        Ok(())
    }

    // ---------------------------------------------------------------- local commands

    /// A local command: `[1, 0, code]` (a pairing waits: the code to show), `[1, 0]` done or
    /// nothing waits,
    /// `[1, 1]` refused (another user), `[1, 2]` no such pairing, `[1, 3]` every browser slot
    /// taken.
    fn local<R: Random, B: Allocator>(
        &mut self,
        conn: LocalId,
        uid: u32,
        ask: Ask,
        now: Nanos,
        random: &mut R,
        out: &mut Out<B>,
    ) -> Result<(), ViewerError> {
        let answer = |out: &mut Out<B>, code: u64, shown: Option<Code>| {
            let span = out.write(|b| {
                cbor::array(b, if shown.is_some() { 3 } else { 2 });
                cbor::uint(b, PAGE_VERSION);
                cbor::uint(b, code);
                if let Some(shown) = shown {
                    cbor::text(b, shown.as_str());
                }
            });
            out.effects.push(Effect::ToLocal { conn, span });
        };
        if uid != self.config.uid {
            answer(out, 1, None);
            return Ok(());
        }
        match ask {
            Ask::Pending => answer(out, 0, self.pairing.shown(now)),
            Ask::Confirm(code) => match self.pairing.confirm(code, now, random) {
                Ok(()) => {
                    let pairing = &self.pairing;
                    let span = out.write(|b| pairing.write_keys(b));
                    out.effects.push(Effect::StoreKeys { span });
                    answer(out, 0, None);
                }
                Err(PairError::NoSuch) => answer(out, 2, None),
                Err(PairError::Full { .. }) => answer(out, 3, None),
            },
            Ask::Decline => {
                self.pairing.decline();
                answer(out, 0, None);
            }
        }
        Ok(())
    }
}

/// A CBOR writer into a fixed array, for the few small answers built without the allocator.
struct Cursor<'b> {
    buf: &'b mut [u8],
    at: usize,
}

impl Cursor<'_> {
    fn head(&mut self, major: u8, n: u64) {
        let major = major << 5;
        let (first, rest): (u8, &[u8]) = match n {
            0..=23 => (major | n as u8, &[]),
            24..=0xff => (major | 24, &[n as u8][..]),
            _ => {
                let len = cbor::head_len(n) - 1;
                let bytes = n.to_be_bytes();
                let code = match len {
                    2 => 25,
                    4 => 26,
                    _ => 27,
                };
                self.buf[self.at] = major | code;
                self.at += 1;
                self.buf[self.at..self.at + len].copy_from_slice(&bytes[8 - len..]);
                self.at += len;
                return;
            }
        };
        self.buf[self.at] = first;
        self.at += 1;
        self.buf[self.at..self.at + rest.len()].copy_from_slice(rest);
        self.at += rest.len();
    }

    fn array(&mut self, len: u64) {
        self.head(4, len);
    }

    fn uint(&mut self, n: u64) {
        self.head(0, n);
    }
}
