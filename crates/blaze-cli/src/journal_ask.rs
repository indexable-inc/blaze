//! The command's side of the checkout's journal helper (end-state.md D168 mechanism 4; the
//! helper is journal_helper.rs, its session and wire blaze-watch helper.rs): the two
//! instantiations of journal_sweep.rs's [`ChangedSince`] seam that ask it, beside `Replay`.
//!
//! **Why a helper.** A replay makes, replays, flushes and stops a stream per command, and the
//! stream's own setup is the cost, not the changes: tempo, twelve edits in a row on binary
//! c0340wave89 (one path per stream, the cookie under the checkout), "journal replayed" 14.5 to
//! 176.4 ms, three of twelve paying "HistoryDone 159.1 / 160.1 / 160.8 ms" with places 2.6 s
//! old while a 196 s old place was fast, and "cookie heard" 0.3 to 3.6 ms or 10.5 to 12.2 ms (7
//! of 13 pay about 11 ms). Neither the paths, the volume nor the place's age decides it: it is
//! fseventsd's own delivery. A stream held open across commands pays neither its start nor its
//! history; what a command pays is one socket round trip and the helper's barrier (one flush of
//! a stream already in place). The row's target: the whole `journal` phase in well under 1 ms
//! plus that barrier, which the `sweep:` line prints ([`HelperRow`]).
//!
//! **The strategies** (rule 78: one seam, each a complete answer):
//!
//! - [`Asked`]: ask the helper; vouched, the unit's names are its names and the places saved
//!   are its pair (its place, and the journal's place of the flush that issued it). Not vouched
//!   (no socket, a stale one, another checkout or build, a deadline, a frame that does not
//!   parse, a folder not the user's alone, or the helper's own `CannotVouch`): the journal is
//!   replayed from the place kept beside the helper's, and the line says why.
//! - [`Both`] (shipped until the switch): ask the helper FIRST, then replay, mark by the union
//!   of the two (so a wrong helper costs stats, never a stale answer), save the helper's pair,
//!   and print where the two differ ([`differ`]): the acceptance on tempo is twelve edits with
//!   both counts 0 on every one.
//!
//! **Why the order and the pair are sound.** A place saved is sound when every row this request
//! reads is read after it: then a write after it is named by the next replay from it (or by the
//! helper's names since the paired place). The ask and the replay both come before any file of
//! the checkout is read, so either's place is sound to save; the helper's place is saved only
//! with ITS journal place (blaze-watch helper.rs `AskAnswer`), never with the replay's end, so a
//! later replay from the journal place and the helper's names from its place answer the same
//! question. Under `Both` the ask comes first, so the replay ends after the helper's journal
//! place, and the union covers every write from the stored place to the place saved.
//!
//! **No helper is started here.** [`HelperStart`] has one value, `None`, until the coordinator
//! has run a helper by hand (`blaze journal-helper`, journal_helper.rs) on tempo: an ask that
//! finds none prints that one would be started, and replays.
//!
//! **What `Both` cannot tell apart.** A write made between the helper's barrier and the
//! replay's (a few ms, the command's own window) is the replay's alone: an only-replay name in
//! that window is not a helper's miss. Its names are printed, so a reader can tell.

use core::time::Duration;

use blaze_own::Vec;
use blaze_watch::Nanos;
use blaze_watch::helper::{self, AskAnswer, AskNext, AskStep, Asking, CheckoutKey, Hello, JournalAt, NotAsked, Version, WireError};
use blaze_watch::since::{self, Place};
use bstd::clock::{Clock, Instant};
use bstd::cx::Cx;
use bstd::fs::{Cursor, Dir, FsError, PrivateError, ReadWrite};
use bstd::local::Local;
use bstd::mem::{Bytes, Frame};
use bstd::net::{Conn, NetError, NetKind};
use bstd::path::Path;

use crate::CliError;
use crate::journal_sweep::{ChangedSince, HELPER_PLACE, HelperRow, JOURNAL_WAIT, Said, Stored, Taken};

// The stored form keeps a helper's place in journal_sweep.rs's bytes: one size.
const _: () = assert!(HELPER_PLACE == Place::BYTES);

/// The digest's domain of a [`CheckoutKey`].
const CHECKOUT_DOMAIN: &str = "blaze-cli journal helper checkout 1";

/// Most bytes one receive asks for: a frame's head and a small answer in one.
const RECV_MOST: usize = 16 << 10;

/// Steps of one ask at most: connect, hello, ask, answer.
const STEPS_MOST: u32 = 8;

/// How long a connection's hang-up is waited for once its ask ended (its deadline may have
/// passed): a Unix socket's close is immediate.
const HANGUP_WAIT: Duration = Duration::from_millis(10);

/// Arguments passed to the detached `blaze` child: `journal-helper` alone (cwd is set to the
/// checkout root, since `blaze journal-helper` takes a relative bstd `Path` or defaults to cwd).
#[cfg(unix)]
const HELPER_ARGS: [&str; 1] = ["journal-helper"];

/// Whether a command whose ask found no helper (or a dead one) starts one.
pub(crate) enum HelperStart<'a> {
    /// No helper is started: the line says one would be.
    None,
    /// Starts `blaze journal-helper` (with cwd set to `root`) detached in its own session
    /// (`/dev/null` stdio, zero inherited descriptors) when no live helper answered.
    #[cfg(unix)]
    Detached {
        spawner: bstd::procs::Spawner,
        exe: &'a str,
        root: &'a str,
        home: Option<&'a str>,
    },
}

impl<'a> HelperStart<'a> {
    /// Detached helper start when this host offers a native spawner and knows its own executable
    /// and the checkout's resolved root; otherwise [`HelperStart::None`].
    #[cfg(unix)]
    pub(crate) fn detached(host: &bstd::host::Host, exe: Option<&'a str>, root: Option<&'a str>, home: Option<&'a str>) -> HelperStart<'a> {
        match (host.spawner(), exe, root) {
            (Ok(spawner), Some(exe), Some(root)) => HelperStart::Detached { spawner, exe, root, home },
            _ => HelperStart::None,
        }
    }

    /// Detached helper start is unavailable off Unix.
    #[cfg(not(unix))]
    pub(crate) fn detached(_host: &bstd::host::Host, _exe: Option<&'a str>, _root: Option<&'a str>, _home: Option<&'a str>) -> HelperStart<'a> {
        HelperStart::None
    }

    /// Starts a detached helper if `outcome` indicates no live helper is serving this checkout.
    fn maybe_start(&self, cx: &Cx, outcome: &Outcome<'_>) -> StartedHelper {
        let wanted = match outcome {
            Outcome::NotAsked { start: true, .. } => true,
            Outcome::Answered { answer: AskAnswer::CannotVouch { why, .. }, .. } => {
                matches!(why, since::CannotVouch::Dead(_) | since::CannotVouch::NoHelper)
            }
            _ => false,
        };
        if !wanted {
            return StartedHelper::NotWanted;
        }
        match self {
            HelperStart::None => StartedHelper::None,
            #[cfg(unix)]
            HelperStart::Detached { spawner, exe, root, home } => {
                let env_pair = home.map(|home| [("HOME", home)]);
                let env: &[(&str, &str)] = match &env_pair {
                    Some(pair) => pair.as_slice(),
                    None => &[],
                };
                let spec = bstd::procs::DetachedSpec {
                    program: exe,
                    args: &HELPER_ARGS,
                    env,
                    dir: root,
                };
                match spawner.start_detached(cx, &spec) {
                    Ok(pid) => StartedHelper::Started(pid),
                    Err(error) => StartedHelper::Failed(error),
                }
            }
        }
    }
}

/// Outcome of attempting to start a detached helper after an unanswered ask.
enum StartedHelper {
    NotWanted,
    None,
    #[cfg(unix)]
    Started(bstd::procs::ProcId),
    #[cfg(unix)]
    Failed(bstd::procs::RunError),
}

/// This command as a helper sees it: its build and its checkout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Me {
    pub(crate) version: Version,
    pub(crate) checkout: CheckoutKey,
    /// The checkout's socket, relative to blaze's folder in the home (blaze-watch helper.rs
    /// `socket_path`: `v2/journal/<16 hex of the key>.sock`).
    pub(crate) socket_path: [u8; helper::SOCKET_PATH_BYTES],
    /// Whether that socket's absolute path fits a socket's address: a command does not
    /// connect, and a helper does not bind, where it does not.
    pub(crate) socket: SocketFits,
}

impl Me {
    /// This build (`host.build_id()`) in the checkout whose root the command's edge resolved
    /// (engine_script_reads.rs `Served::at_edge`, `root_text`; `None` in a simulated world),
    /// with `HOME` as the program's start read it (`None`: no home, no socket to fit).
    pub(crate) fn of(build: &bstd::host::BuildId, root: Option<&str>, home: Option<&str>, frame: &Frame) -> Me {
        let version = Version::of(build.as_bytes()).expect("a build id is 1 to 32 bytes, as a helper's version");
        let checkout = checkout_key(root, frame);
        Me { version, checkout, socket_path: helper::socket_path(&checkout), socket: SocketFits::of(home) }
    }

    /// [`Me::socket_path`] as a path beneath blaze's folder in the home.
    pub(crate) fn socket_beneath_home(&self) -> Result<Path, CliError> {
        let text = core::str::from_utf8(&self.socket_path).map_err(|_| CliError::Text { what: "the helper's socket's name is not UTF-8 (blaze-watch helper.rs `socket_path`)" })?;
        Ok(Path::new(text)?)
    }
}

/// Whether `<HOME>/.blaze/v2/journal/<16 hex>.sock` (blaze-watch helper.rs `socket_path`,
/// under store.rs `HOME_FOLDER`), its NUL counted, fits a local socket's path
/// (`bstd::local::SOCKET_PATH_MOST`: `sun_path`, 104 bytes on macOS). Its length is the home's
/// plus a constant ([`PAST_HOME`]: 40 bytes and the NUL), whatever the checkout's depth: a home
/// of 63 bytes or fewer fits (a macOS home is 7 bytes and the name). Checked from the home's text before any connect or bind, both sides by this one
/// function: a home too long for one is said as itself (`TooLong`, naming the path and the
/// limit), never as "no socket" or "a stale socket", which is what the connect's failure would
/// look like, and no helper is offered, since none could listen there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SocketFits {
    Fits,
    /// The socket's absolute path would be `bytes` long with its NUL; the home is `home` bytes.
    TooLong { bytes: usize, home: usize },
}

/// Bytes of a socket's absolute path past the home's own: `/`, blaze's folder, `/`, the socket
/// beneath it, the NUL.
const PAST_HOME: usize = 1 + crate::store::HOME_FOLDER.len() + 1 + helper::SOCKET_PATH_BYTES + 1;

impl SocketFits {
    /// The fit under the home whose text is `home` (`None`: no home, no path to fit).
    pub(crate) fn of(home: Option<&str>) -> SocketFits {
        let Some(home) = home else { return SocketFits::Fits };
        let home = home.trim_end_matches('/').len();
        let bytes = home + PAST_HOME;
        match bytes <= bstd::local::SOCKET_PATH_MOST {
            true => SocketFits::Fits,
            false => SocketFits::TooLong { bytes, home },
        }
    }
}

impl core::fmt::Display for SocketFits {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SocketFits::Fits => write!(f, "the helper's socket under the home's {}/{} fits a socket's path", crate::store::HOME_FOLDER, helper::SOCKET_FOLDER),
            SocketFits::TooLong { bytes, home } => write!(
                f,
                "the helper's socket <HOME>/{}/{}/<{} bytes>, under a home of {home} bytes, is {bytes} bytes with its NUL, over the {} a local socket's path may hold (`sun_path`; journal_ask.rs `SocketFits`)",
                crate::store::HOME_FOLDER,
                helper::SOCKET_FOLDER,
                helper::SOCKET_NAME_BYTES,
                bstd::local::SOCKET_PATH_MOST
            ),
        }
    }
}

/// Allocation-free checkout key computation matching [`checkout_key`], shared by the pre-runtime
/// daemon client (`daemon_client.rs`), resident daemon (`daemon_server.rs`), and `clean.rs`.
pub(crate) fn checkout_key_of(root: &str) -> CheckoutKey {
    let mut hasher = blaze_blake3::Hasher::new();
    hasher.update(CHECKOUT_DOMAIN.as_bytes());
    hasher.update(b"\n");
    hasher.update(root.as_bytes());
    CheckoutKey(hasher.finalize())
}

/// The key a helper and a command both compute for one checkout: the digest of its root as one
/// `realpath` resolved it (the helper's edge and the command's make the same call), so a socket
/// reached through a link to another checkout is refused by its hello. `None` (a simulated
/// world, or a root that is not UTF-8): the key of no root.
pub(crate) fn checkout_key(root: Option<&str>, _frame: &Frame) -> CheckoutKey {
    checkout_key_of(root.unwrap_or(""))
}

/// The helper's place the stored answer kept, when it kept the journal place it pairs with
/// (the checkout unit's): else none is asked from (an ask from no place is refused `NoPlace`
/// and issues one).
fn paired_place(stored: Stored<'_>) -> Option<Place> {
    let checkout = stored.units().any(|(at, name, _)| at == crate::journal_sweep::Where::Checkout as u8 && name.is_empty());
    checkout.then(|| stored.helper_place()).flatten().and_then(|bytes| Place::decode(&bytes))
}

/// The command asks the helper ([`ChangedSince`]): the module's `Asked`.
pub(crate) struct Asked<'a> {
    pub(crate) me: Me,
    pub(crate) start: HelperStart<'a>,
}

/// The command asks the helper, then replays, and compares: the module's `Both`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Both<'a> {
    pub(crate) me: Me,
    pub(crate) start: HelperStart<'a>,
}

/// The strategy the command ships with: ask the detached helper directly, falling back to a
/// replay only when no helper vouches.
pub(crate) type Shipped<'a> = Asked<'a>;

impl ChangedSince for Asked<'_> {
    const NAME: &'static str = "asked";

    fn changed_since<'f>(
        &self, cx: &Cx, root: &Dir<ReadWrite>, home: &Dir<ReadWrite>, clock: &Clock, stored: Stored<'_>, frame: &'f Frame,
    ) -> Result<Taken<'f>, CliError> {
        let cx = cx.span("sweep: the changes since the stored answer's place, asked of the journal helper");
        let cx = cx.until(clock.after(JOURNAL_WAIT));
        let mut taken = Taken::begin(&cx, root, clock, frame)?;
        let asked = ask(&cx, home, clock, self.me, paired_place(stored), frame)?;
        let names = match asked.outcome {
            Outcome::Answered { answer: AskAnswer::Vouched { place, journal, .. }, names } => match Cursor::decode(journal.as_bytes()) {
                Some(cursor) => {
                    taken.vouched(stored, names, cursor);
                    taken.pair_with_helper(cursor, place.encode());
                    Some(names)
                }
                None => {
                    // A journal place that is not a cursor: the answer cannot be paired.
                    taken.replay_checkout(&cx, clock, root, stored);
                    None
                }
            },
            Outcome::Answered { answer: AskAnswer::CannotVouch { place, journal, .. }, .. } => {
                taken.replay_checkout(&cx, clock, root, stored);
                pair_refused(&mut taken, place, journal);
                None
            }
            Outcome::NotAsked { .. } | Outcome::NotPrivate(_) | Outcome::TooLong(_) => {
                taken.replay_checkout(&cx, clock, root, stored);
                None
            }
        };
        let started = self.start.maybe_start(&cx, &asked.outcome);
        let counted = names.map(|names| u32::try_from(names.len()).unwrap_or(u32::MAX));
        taken.said(HelperRow { by: Self::NAME, ask_ns: asked.ns, names: counted, differ: None, said: said_of(&asked, &started, None) });
        Ok(taken)
    }
}

impl ChangedSince for Both<'_> {
    const NAME: &'static str = "both: asked, then replayed";

    fn changed_since<'f>(
        &self, cx: &Cx, root: &Dir<ReadWrite>, home: &Dir<ReadWrite>, clock: &Clock, stored: Stored<'_>, frame: &'f Frame,
    ) -> Result<Taken<'f>, CliError> {
        let cx = cx.span("sweep: the changes since the stored answer's place, asked of the journal helper and replayed");
        let cx = cx.until(clock.after(JOURNAL_WAIT));
        let mut taken = Taken::begin(&cx, root, clock, frame)?;
        // The ask FIRST: the replay then ends after the helper's journal place (the module's
        // doc), so the union covers every write up to the place saved.
        let asked = ask(&cx, home, clock, self.me, paired_place(stored), frame)?;
        taken.replay_checkout(&cx, clock, root, stored);
        let started = self.start.maybe_start(&cx, &asked.outcome);
        // The differential is read before the helper's names join the unit: it borrows the
        // replay's names from `taken`.
        let vouched = match asked.outcome {
            Outcome::Answered { answer: AskAnswer::Vouched { .. }, names } => Some(names),
            _ => None,
        };
        let (differ_counts, said) = {
            let differs = vouched.zip(taken.replayed_names()).map(|(names, replayed)| differ(names, replayed, frame));
            let counts = differs.as_ref().map(|it| (u32::try_from(it.only_helper.len()).unwrap_or(u32::MAX), u32::try_from(it.only_replay.len()).unwrap_or(u32::MAX)));
            (counts, said_of(&asked, &started, differs.as_ref()))
        };
        match asked.outcome {
            Outcome::Answered { answer: AskAnswer::Vouched { place, journal, .. }, names } => {
                taken.add_helper_names(names);
                if let Some(cursor) = Cursor::decode(journal.as_bytes()) {
                    taken.pair_with_helper(cursor, place.encode());
                }
            }
            Outcome::Answered { answer: AskAnswer::CannotVouch { place, journal, .. }, .. } => pair_refused(&mut taken, place, journal),
            Outcome::NotAsked { .. } | Outcome::NotPrivate(_) | Outcome::TooLong(_) => {}
        }
        let counted = vouched.map(|names| u32::try_from(names.len()).unwrap_or(u32::MAX));
        taken.said(HelperRow { by: Self::NAME, ask_ns: asked.ns, names: counted, differ: differ_counts, said });
        Ok(taken)
    }
}

/// A refusal that issued a place: it is saved with ITS journal place (never the replay's end).
fn pair_refused(taken: &mut Taken<'_>, place: Option<Place>, journal: JournalAt) {
    if let (Some(place), Some(cursor)) = (place, Cursor::decode(journal.as_bytes())) {
        taken.pair_with_helper(cursor, place.encode());
    }
}

/// What came of one ask.
enum Outcome<'f> {
    /// The helper answered; `names` (vouched) in the request's frame.
    Answered { answer: AskAnswer, names: &'f [&'f str] },
    /// No helper answered ([`NotAsked`]); `start`: one would be started.
    NotAsked { why: NotAsked, start: bool },
    /// The socket's folder is not the user's alone: nothing in it is trusted.
    NotPrivate(PrivateError),
    /// The socket's path does not fit a socket's address under this checkout ([`SocketFits`]):
    /// nothing was connected to, and no helper is offered.
    TooLong(SocketFits),
}

/// What the driver saw beyond the outcome: the error under a [`NotAsked`], for the line.
enum Detail {
    None,
    Net(NetError),
    Wire(WireError),
    Fs(FsError),
}

/// One ask, timed.
struct AskedOnce<'f> {
    outcome: Outcome<'f>,
    detail: Detail,
    /// The connection's hang-up, when there was a connection and it failed.
    hangup: Option<NetError>,
    ns: u64,
}

/// The ask's driver (blaze-watch helper.rs `Asking`, sans-IO, decides each step): the socket's
/// folder under blaze's folder in the home (`home`) made the user's alone, connect, hello,
/// ask, answer, all under `helper::ASK_DEADLINE`. Never an error: every way it ends is an
/// [`Outcome`] the line prints, and the caller replays. `Err` only for the socket's path, made
/// from a constant and the checkout's key.
fn ask<'f>(cx: &Cx, home: &Dir<ReadWrite>, clock: &Clock, me: Me, since: Option<Place>, frame: &'f Frame) -> Result<AskedOnce<'f>, CliError> {
    let folder = Path::new(helper::SOCKET_FOLDER)?;
    let socket = me.socket_beneath_home()?;
    let outer = cx;
    let cx = cx.span("journal: the helper asked (its folder checked, connect, hello, ask, answer)");
    let born = clock.now();
    let cx = cx.until(clock.after(Duration::from_nanos(helper::ASK_DEADLINE.0)));
    let took = |clock: &Clock| clock.now().since(born).map_or(0, |took| u64::try_from(took.as_nanos()).unwrap_or(u64::MAX));
    if let SocketFits::TooLong { .. } = me.socket {
        return Ok(AskedOnce { outcome: Outcome::TooLong(me.socket), detail: Detail::None, hangup: None, ns: took(clock) });
    }
    let (mut asking, _connect) = Asking::new(me.version, me.checkout, since, Nanos(0));
    if let Err(why) = home.make_private(&cx, &folder) {
        return Ok(AskedOnce { outcome: Outcome::NotPrivate(why), detail: Detail::None, hangup: None, ns: took(clock) });
    }
    let conn = match Local::beneath(home).connect(&cx, &socket) {
        Ok(conn) => conn,
        Err(error) => {
            let (why, detail) = match &error.kind {
                NetKind::Stopped(_) => (NotAsked::Deadline { at: AskStep::Connect }, Detail::Net(error)),
                _ => match home.stat(&cx, &socket) {
                    Ok(None) => (NotAsked::NoSocket, Detail::None),
                    Ok(Some(_)) => (NotAsked::Stale, Detail::Net(error)),
                    Err(stat) => (NotAsked::Broken { at: AskStep::Connect }, Detail::Fs(stat)),
                },
            };
            let outcome = match asking.connected(Err(why)) {
                AskNext::Replay { why, start } => Outcome::NotAsked { why, start },
                _ => Outcome::NotAsked { why, start: true },
            };
            return Ok(AskedOnce { outcome, detail, hangup: None, ns: took(clock) });
        }
    };
    let (outcome, detail) = converse(&cx, &conn, &mut asking, clock, born, frame);
    // The connection ends on every path (it is linear), under a short deadline of its own: the
    // ask's may have passed.
    let hangup = conn.finish(&outer.until(clock.after(HANGUP_WAIT))).err();
    Ok(AskedOnce { outcome, detail, hangup, ns: took(clock) })
}

/// The ask on an open connection, step by step as [`Asking`] says.
fn converse<'f>(cx: &Cx, conn: &Conn, asking: &mut Asking, clock: &Clock, born: Instant, frame: &'f Frame) -> (Outcome<'f>, Detail) {
    let now = || Nanos(clock.now().since(born).map_or(0, |took| u64::try_from(took.as_nanos()).unwrap_or(u64::MAX)));
    let mut buf = Bytes::new_in(frame, "the journal helper's frames");
    let mut next = asking.connected(Ok(()));
    let mut hello_read = false;
    let mut names: &'f [&'f str] = &[];
    let mut detail = Detail::None;
    for _ in 0..STEPS_MOST {
        next = match next {
            // Only the first step connects, and it did.
            AskNext::Connect => asking.broke(),
            AskNext::Read => match read_frame(cx, conn, &mut buf) {
                Err(ReadFail::Stopped(error)) => {
                    detail = Detail::Net(error);
                    asking.tick(now()).unwrap_or_else(|| asking.broke())
                }
                Err(ReadFail::Net(error)) => {
                    detail = Detail::Net(error);
                    asking.broke()
                }
                Err(ReadFail::Wire(error)) => {
                    detail = Detail::Wire(error);
                    asking.broke()
                }
                Err(ReadFail::HungUp) => asking.broke(),
                Ok(body) if !hello_read => {
                    hello_read = true;
                    match Hello::decode(body) {
                        Ok(hello) => asking.hello(&hello),
                        Err(error) => {
                            detail = Detail::Wire(error);
                            asking.broke()
                        }
                    }
                }
                Ok(body) => {
                    // The names borrow the body: kept in the request's frame.
                    let body: &'f [u8] = frame.alloc_slice_copy(body);
                    let mut told = Vec::new_in(frame);
                    match helper::decode_answer(body, &mut told) {
                        Ok(answer) => {
                            names = told.into_bump_slice();
                            asking.answered(answer)
                        }
                        Err(error) => {
                            detail = Detail::Wire(error);
                            asking.broke()
                        }
                    }
                }
            },
            AskNext::Send(ask) => {
                let mut out = Vec::new_in(frame);
                ask.encode(&mut out);
                match conn.send_all(cx, &out) {
                    Ok(()) => AskNext::Read,
                    Err(error) => {
                        let stopped = matches!(error.kind, NetKind::Stopped(_));
                        detail = Detail::Net(error);
                        match stopped {
                            true => asking.tick(now()).unwrap_or_else(|| asking.broke()),
                            false => asking.broke(),
                        }
                    }
                }
            }
            AskNext::Answered(answer) => return (Outcome::Answered { answer, names }, detail),
            AskNext::Replay { why, start } => return (Outcome::NotAsked { why, start }, detail),
        };
    }
    // More steps than an ask has: the driver's own bug, said as a broken ask (rule 18).
    match asking.broke() {
        AskNext::Replay { why, start } => (Outcome::NotAsked { why, start }, detail),
        _ => (Outcome::NotAsked { why: NotAsked::Broken { at: AskStep::Answer }, start: false }, detail),
    }
}

/// Why a frame was not read (here, and by the helper's driver, journal_helper.rs).
pub(crate) enum ReadFail {
    /// The connection's deadline passed.
    Stopped(NetError),
    Net(NetError),
    Wire(WireError),
    /// The other end hung up before the frame was whole, or sent more than one frame.
    HungUp,
}

impl core::fmt::Display for ReadFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReadFail::Stopped(error) | ReadFail::Net(error) => write!(f, "{error}"),
            ReadFail::Wire(error) => write!(f, "{error}"),
            ReadFail::HungUp => f.write_str("the other end hung up before its frame was whole, or sent more than one"),
        }
    }
}

/// One frame read whole (its length word, then its body): the body. Each side sends one frame
/// and waits for the other's, so bytes past it are refused.
pub(crate) fn read_frame<'b>(cx: &Cx, conn: &Conn, buf: &'b mut Bytes<'_>) -> Result<&'b [u8], ReadFail> {
    buf.clear();
    let mut want = 4;
    let mut whole = false;
    loop {
        if !whole && let Some(len) = helper::frame_len(buf.as_slice()) {
            want = 4 + len.map_err(ReadFail::Wire)?;
            whole = true;
        }
        if whole && buf.len() >= want {
            break;
        }
        let most = (want - buf.len()).max(RECV_MOST);
        match conn.recv(cx, most, buf) {
            Ok(0) => return Err(ReadFail::HungUp),
            Ok(_) => {}
            Err(error) if matches!(error.kind, NetKind::Stopped(_)) => return Err(ReadFail::Stopped(error)),
            Err(error) => return Err(ReadFail::Net(error)),
        }
    }
    // The helper sends one frame and waits for the next ask: bytes past it are not of this
    // form.
    if buf.len() != want {
        return Err(ReadFail::HungUp);
    }
    Ok(&buf.as_slice()[4..want])
}

/// Where the helper's names and the replay's differ (`Both`), each side's names that no name of
/// the other covers (blaze-watch since.rs `covers`: a name, or a folder above it). Left out of
/// both: the checkout's own entry (`""`) and blaze's cookie and socket folders, which each
/// side writes for itself.
pub(crate) struct Differ<'a, 'f> {
    pub(crate) only_helper: Vec<&'a str, &'f Frame>,
    pub(crate) only_replay: Vec<&'a str, &'f Frame>,
}

/// Names of blaze's own traffic, compared by neither side: the replay's and the helper's cookies
/// (blaze-core fsevents.rs `COOKIE_AREA`). The helper's socket is under the home since
/// 2026-11-21 (blaze-watch helper.rs `SOCKET_FOLDER`), no name of the checkout's.
#[cfg_attr(not(test), allow(dead_code))]
const OWN_TRAFFIC: [&str; 1] = [".blaze/v2/fsevents"];

#[cfg_attr(not(test), allow(dead_code))]
fn compared(name: &str) -> bool {
    !name.is_empty() && !OWN_TRAFFIC.iter().any(|own| since::covers(own, name))
}

/// A bug planted in [`differ`] for its test's control (tests/journal_ask_tests.rs).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DifferPlant {
    None,
    /// A name is covered only by itself, not by a folder above it.
    NamesOnly,
}

/// [`Differ`] of the helper's `helper` and the replay's `replay` (sans-IO).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn differ<'a, 'f>(helper: &[&'a str], replay: impl Iterator<Item = &'a str>, frame: &'f Frame) -> Differ<'a, 'f> {
    differ_planted(helper, replay, frame, DifferPlant::None)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn differ_planted<'a, 'f>(helper: &[&'a str], replay: impl Iterator<Item = &'a str>, frame: &'f Frame, plant: DifferPlant) -> Differ<'a, 'f> {
    let is_covered = |sorted: &[&str], path: &str| match plant {
        DifferPlant::None => covered(sorted, path),
        DifferPlant::NamesOnly => sorted.binary_search(&path).is_ok(),
    };
    let mut theirs: Vec<&'a str, &'f Frame> = Vec::new_in(frame);
    theirs.extend(helper.iter().copied().filter(|name| compared(name)));
    theirs.sort_unstable();
    theirs.dedup();
    let mut ours: Vec<&'a str, &'f Frame> = Vec::new_in(frame);
    ours.extend(replay.filter(|name| compared(name)));
    ours.sort_unstable();
    ours.dedup();
    let mut only_helper = Vec::new_in(frame);
    only_helper.extend(theirs.iter().copied().filter(|name| !is_covered(&ours, name)));
    let mut only_replay = Vec::new_in(frame);
    only_replay.extend(ours.iter().copied().filter(|name| !is_covered(&theirs, name)));
    Differ { only_helper, only_replay }
}

/// Whether `path`, or a folder above it, is in `sorted`.
#[cfg_attr(not(test), allow(dead_code))]
fn covered(sorted: &[&str], path: &str) -> bool {
    sorted.binary_search(&path).is_ok() || path.char_indices().filter(|(_, c)| *c == '/').any(|(at, _)| sorted.binary_search(&&path[..at]).is_ok())
}

/// Names of each side printed at most.
const SHOWN: usize = 3;

/// What the line says of one ask.
fn said_of(asked: &AskedOnce<'_>, started: &StartedHelper, differs: Option<&Differ<'_, '_>>) -> Said {
    let mut said = Said::of(format_args!(""));
    let _cut = write_said(&mut said, asked, started, differs);
    said
}

fn write_said(out: &mut Said, asked: &AskedOnce<'_>, started: &StartedHelper, differs: Option<&Differ<'_, '_>>) -> core::fmt::Result {
    use core::fmt::Write as _;
    match &asked.outcome {
        Outcome::Answered { answer: AskAnswer::Vouched { since: from, place, journal }, .. } => {
            write!(out, "from place {from} to {place} (its journal's place {} bytes)", journal.as_bytes().len())?
        }
        Outcome::Answered { answer: AskAnswer::CannotVouch { why, place, .. }, .. } => {
            write!(out, "{why}; replayed from the journal's place kept beside the helper's")?;
            if let Some(place) = place {
                write!(out, "; saved its new place {place}")?;
            }
        }
        Outcome::NotAsked { why, .. } => {
            write!(out, "not asked: {why}; replayed")?;
        }
        Outcome::NotPrivate(why) => write!(out, "not asked: {why}; replayed")?,
        Outcome::TooLong(fits) => write!(out, "not asked: {fits}; replayed (no helper can listen there)")?,
    }
    match started {
        StartedHelper::NotWanted => {}
        StartedHelper::None => out.write_str("; a helper would be started (HelperStart::None: none is; `blaze journal-helper` runs one)")?,
        #[cfg(unix)]
        StartedHelper::Started(pid) => write!(out, "; started detached journal helper (pid {pid})")?,
        #[cfg(unix)]
        StartedHelper::Failed(error) => write!(out, "; starting detached journal helper failed: {error}")?,
    }
    match &asked.detail {
        Detail::None => {}
        Detail::Net(error) => write!(out, " ({error})")?,
        Detail::Wire(error) => write!(out, " ({error})")?,
        Detail::Fs(error) => write!(out, " ({error})")?,
    }
    if let Some(error) = &asked.hangup {
        write!(out, "; its hang-up failed: {error}")?;
    }
    if let Some(differs) = differs {
        for (side, names) in [("only the helper's", &differs.only_helper), ("only the replay's", &differs.only_replay)] {
            if names.is_empty() {
                continue;
            }
            write!(out, "; {side}:")?;
            for name in names.iter().take(SHOWN) {
                write!(out, " {name}")?;
            }
            if names.len() > SHOWN {
                write!(out, " and {} more", names.len() - SHOWN)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/journal_ask_tests.rs"]
mod tests;
