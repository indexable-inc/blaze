//! `blaze journal-helper [folder]`: the checkout's journal helper, the driver of blaze-watch
//! helper.rs's [`Helper`] (end-state.md D168 mechanism 4). It holds ONE journal stream of the
//! checkout open for its life (bstd `Dir::hold_changes`), so a command's ask (journal_ask.rs)
//! costs one socket round trip and one flush of a stream already in place: never a stream's
//! start, its history, its `HistoryDone` (tempo, binary c0340wave89: 159.1 to 160.8 ms on three
//! of twelve edits) or its end.
//!
//! **Its life** (rule 70: a helper has an expiry and holds only what the journal rebuilds):
//! the stream started and caught up, the core made with its first place issued behind a flush,
//! then the socket (`<HOME>/.blaze/v2/journal/<16 hex of the checkout's key>.sock`, in a
//! folder made the user's alone: blaze-watch helper.rs `SOCKET_FOLDER`, `socket_path`) bound:
//! a command that connects finds a helper that can answer. Then, one connection at a time
//! (an ask is one flush; commands of one checkout seldom overlap, and one waiting is bounded
//! by its own `ASK_DEADLINE`): hello, the ask, the core's step (a flush when it says), the
//! answer with its names, the connection ended. Between asks, a tick a second takes what the
//! stream told and the core's expiry. It ends when the core says ([`Death`]: idle 30 min, held
//! 12 h, a blaze of another build asked, the checkout gone, the stream failed or dropped
//! changes): the socket closed, the stream ended, why printed.
//!
//! **Nothing starts it yet.** It is run by hand (`blaze journal-helper` in the checkout) until
//! the coordinator has run it on tempo: journal_ask.rs `HelperStart` has the one value `None`.
//!
//! **What it is told.** A batch's names go to the core, but the checkout's own entry (`""`: its
//! folder's times change with every file made in it) and blaze's own traffic are names like any
//! other or skipped: `""` is skipped (the core reads an empty name as the root changed), the
//! rest kept (they cost a name each, and the command leaves them out of its differential). A
//! lost subtree is named by its folder (a name covers what is under it); a loss of the whole
//! checkout ends the helper: the root moved ([`Fault::RootChanged`]), events dropped or to be
//! rescanned ([`Fault::Overflowed`]), the journal reset or cut ([`Fault::WatchFailed`]).
//!
//! **Memory.** The core's tables are made once in a frame of their own (its `Config`: 64 Ki
//! names of 8 MiB at most, twice reserved); each connection's bytes live in a second frame
//! reset after it ([`Frame::scope`]): a helper's footprint is flat over its life.

use core::fmt;
use core::time::Duration;
use std::sync::Arc;

use blaze_own::Vec;
use blaze_proc_macro::script_sandbox::RootUnresolved;
use blaze_watch::Nanos;
use blaze_watch::helper::{self, Ask, Helper, JournalAt};
use blaze_watch::since::{self, Answer, AskId, Death, Fault, SinceError, Step};
use bstd::args::Args;
use bstd::clock::{Clock, Instant};
use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::fs::{Changes, Cursor, Dir, Fs, FsError, HeldChanges, HomeError, JournalError, Lost, PrivateError, ReadWrite, Stamp};
use bstd::host::{BuildId, build_id_in};
use bstd::Stopped;
use bstd::local::Local;
use bstd::mem::{Bytes, Frame, MemError, Tag};
use bstd::net::{Conn, Listener, NetError, NetKind};
use bstd::out::Out;
use bstd::path::{Path, PathError};

use crate::CliError;
use crate::journal_ask::{Me, ReadFail, SocketFits, read_frame};

// A cursor's bytes fit a journal place, so `journal_at` never refuses one.
const _: () = assert!(Cursor::BYTES <= JournalAt::MOST);

/// The stream's start, to its history caught up: a fresh stream's history is empty, so this
/// waits for one delivery (the ~160 ms quantum at most, measured) with room.
const START_WAIT: Duration = Duration::from_secs(5);

/// One flush of the held stream: its cookie written and heard (0.3 to 12.2 ms measured on
/// tempo); past this the stream is taken as failed and the helper ends.
const FLUSH_WAIT: Duration = Duration::from_secs(2);

/// How long an accept waits before the helper takes what the stream told and ticks its expiry.
const TICK: Duration = Duration::from_secs(1);

/// One connection's reads and writes: a command gives up at `helper::ASK_DEADLINE` (50 ms);
/// a little more, so its hang-up is seen as one.
const CONN_WAIT: Duration = Duration::from_millis(100);

/// The socket closed and the stream ended, at the helper's end.
const END_WAIT: Duration = Duration::from_secs(2);

/// Flushes one call of the core may ask in a row (each completes the last): past this the core
/// is in a loop, a bug said as one (rule 18).
const FLUSHES_MOST: u32 = 4;

/// Why the helper could not start, or stopped other than by its core's word.
#[derive(Debug)]
pub(crate) enum HelperError {
    Usage,
    /// The checkout's root has no absolute path (engine_script_reads.rs `Served::at_edge`).
    Root(RootUnresolved),
    /// The socket's folder is not the user's alone: nothing is bound in it.
    Folder(PrivateError),
    Memory(MemError),
    /// The stream did not start (Linux keeps no journal yet: `NoJournal`).
    Stream(JournalError),
    /// The socket's path does not fit a socket's address under this home (journal_ask.rs
    /// `SocketFits`): checked from the home's text before the stream starts.
    SocketTooLong(SocketFits),
    /// Its own executable could not be looked at, at its start ([`OwnExe::open`]).
    OwnExe { exe: Arc<str>, why: OwnExeWhy },
    /// The socket was not bound: another helper answers there.
    Listen(NetError),
    Accept(NetError),
    /// The core refused a call: the driver's bug.
    Core(SinceError),
    /// The core asked flushes in a loop, or gave no answer to an ask it took.
    Loop { asks: u64 },
}

impl fmt::Display for HelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = "journal_helper.rs";
        match self {
            HelperError::Usage => f.write_str("`blaze journal-helper` takes one folder, the checkout's, or none"),
            HelperError::Root(error) => write!(f, "the journal helper: the checkout's root: {error} ({at})"),
            HelperError::Folder(error) => write!(f, "the journal helper: its socket's folder, the home's {}/{}: {error} ({at})", crate::store::HOME_FOLDER, helper::SOCKET_FOLDER),
            HelperError::Memory(error) => write!(f, "the journal helper: its frames: {error} ({at})"),
            HelperError::Stream(error) => write!(f, "the journal helper: the checkout's stream did not start: {error} ({at}; bstd `Dir::hold_changes`)"),
            HelperError::SocketTooLong(fits) => write!(f, "the journal helper: {fits}: nothing bound ({at})"),
            HelperError::OwnExe { exe, why } => write!(f, "the journal helper: its own executable {exe}, watched for a replacement: {why} ({at} `OwnExe::open`)"),
            HelperError::Listen(error) => write!(f, "the journal helper: its socket in the home's {}/{} not bound (another helper answers there): {error} ({at})", crate::store::HOME_FOLDER, helper::SOCKET_FOLDER),
            HelperError::Accept(error) => write!(f, "the journal helper: accepting an ask: {error} ({at})"),
            HelperError::Core(error) => write!(f, "the journal helper: its core refused a call, the driver's bug: {error} ({at}; blaze-watch helper.rs)"),
            HelperError::Loop { asks } => write!(f, "the journal helper: after {asks} ask(s), its core asked flushes past {FLUSHES_MOST} in a row or kept an answer: the driver's bug ({at})"),
        }
    }
}

/// `blaze journal-helper [folder]`: the helper of the checkout at `folder` (or here), until its
/// core ends it.
#[expect(clippy::too_many_arguments, reason = "the journal helper's edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    clock: &Clock,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    env: Option<&bstd::env::Env>,
    args: &Args,
    out: &Out,
) -> Result<Status, CliError> {
    let mut dir = None;
    for word in args.each().skip(2) {
        match word {
            _ if word.starts_with('-') || dir.is_some() => return Err(HelperError::Usage.into()),
            folder => dir = Some(folder),
        }
    }
    let frame = cx.frame();
    // The checkout as `check` resolves it (engine_check.rs `edge`): the workspace's root, and
    // its absolute path by the one realpath; the key a command computes from the same call.
    let checkout = crate::checkout::checkout(dir, frame)?;
    let located = crate::run_package::locate(cx, fs, &checkout, frame)?;
    let build = host.build_id()?;
    let served = crate::engine_script_reads::Served::at_edge(cx.is_simulated(), located.at, frame).map_err(HelperError::Root)?;
    // `HOME` as the program's start read it (rule 75): the socket is under blaze's folder in
    // it, as a command's ask reaches it (journal_ask.rs `Me::of`, engine_check.rs).
    let home_text = env.and_then(|env| env.get(&bstd::env::VarName::new("HOME").ok()?).ok().flatten());
    let home = crate::store::Home::of(cx, fs, home_text)?;
    let me = Me::of(&build, served.root_text(), home_text, frame);
    let socket_text = core::str::from_utf8(&me.socket_path).unwrap_or("");
    if let SocketFits::TooLong { .. } = me.socket {
        // The whole path named on its own line: the error says the counts and the limit.
        out.line(cx, format_args!("journal helper: {}/{}/{socket_text} cannot be a socket's path", home_text.unwrap_or(""), crate::store::HOME_FOLDER))?;
        return Err(HelperError::SocketTooLong(me.socket).into());
    }
    // Its own executable, looked at on each tick: a replaced build ends it.
    let exe = host.exe()?;
    let mut own = OwnExe::open(cx, fs, &exe, build).map_err(|why| HelperError::OwnExe { exe: Arc::clone(&exe), why })?;
    // A new instance per start: a place another helper issued is refused by it (since.rs
    // `CannotVouch::OtherInstance`).
    let instance = rng.next_u64();
    let paths = (Path::new(helper::SOCKET_FOLDER)?, me.socket_beneath_home()?);
    let death = serve(cx, &located.root, home.dir(), clock, me, instance, (&paths.0, &paths.1), &mut own, out)?;
    out.line(cx, format_args!("journal helper: ended: {death}"))?;
    Ok(Status::OK)
}

/// The helper's life, from its stream's start to its core's [`Death`]: the stream of the
/// checkout at `root`, the socket in blaze's folder in the home, `home`.
fn serve(
    cx: &Cx, root: &Dir<ReadWrite>, home: &Dir<ReadWrite>, clock: &Clock, me: Me, instance: u64, (folder, socket): (&Path, &Path), own: &mut OwnExe, out: &Out,
) -> Result<Death, CliError> {
    let cx = cx.span("journal helper: its life");
    let born = clock.now();
    // The folder is checked (made, or found the user's alone) before anything is bound in it;
    // the socket is bound through blaze's folder in the home, beneath it.
    let _folder = home.make_private(&cx, folder).map_err(HelperError::Folder)?;
    let mut held = root.hold_changes(&cx.until(clock.after(START_WAIT))).map_err(HelperError::Stream)?;
    let tables = Frame::new(Tag::Other).map_err(HelperError::Memory)?;
    let mut conns = Frame::new(Tag::Other).map_err(HelperError::Memory)?;
    let mut core = Helper::new_in(since::Config::new(), me.version, me.checkout, instance, nanos(clock, born), &tables).map_err(HelperError::Core)?;
    let mut driver = Driver { held: &mut held, clock, born, asks: 0, last_flush_ns: 0 };
    // The core is made with its first flush out (helper.rs `Helper::new_in`).
    let first = driver.steps(&cx, &mut core, Step { flush: true, exit: None });
    let lived = match first {
        Ok(Some(death)) => Ok(death),
        Err(error) => Err(error),
        Ok(None) => match Local::beneath(home).listen(&cx, socket) {
            Err(error) => Err(HelperError::Listen(error).into()),
            Ok(mut listener) => {
                // The listener is linear: its life is one call that returns, and it is closed
                // after, whatever that call returned.
                let lived = live(&cx, &mut listener, &mut driver, &mut core, &mut conns, instance, own, out);
                let closed = {
                    let cx = cx.span("journal helper: its socket closed");
                    listener.close(&cx.until(clock.after(END_WAIT)))
                };
                if let Err(error) = closed {
                    out.line(&cx, format_args!("journal helper: closing the home's {}/{}: {error}", crate::store::HOME_FOLDER, socket.as_str()))?;
                }
                lived
            }
        },
    };
    let ended = {
        let cx = cx.span("journal helper: its stream ended");
        held.end(&cx.until(clock.after(END_WAIT)))
    };
    let death = lived?;
    if let Err(error) = ended {
        out.line(&cx, format_args!("journal helper: ending the stream: {error}"))?;
    }
    Ok(death)
}

/// The helper's life on its bound socket, to its core's [`Death`]: each turn, what the stream
/// told and the expiry, then one accept (a tick a second) and its ask answered. A connection is
/// open only between its accept and its `finish`, with nothing between that can return.
fn live(
    cx: &Cx, listener: &mut Listener, driver: &mut Driver<'_, '_>, core: &mut Helper<&Frame>, conns: &mut Frame, instance: u64, own: &mut OwnExe,
    out: &Out,
) -> Result<Death, CliError> {
    out.line(cx, format_args!("journal helper: answering in the home's {}/{} (instance {instance:016x})", crate::store::HOME_FOLDER, helper::SOCKET_FOLDER))?;
    loop {
        let told = driver.take(cx, core)?;
        if let Some(death) = driver.steps(cx, core, told)? {
            return Ok(death);
        }
        let ticked = core.tick(driver.now()).map_err(HelperError::Core)?;
        if let Some(death) = driver.steps(cx, core, ticked)? {
            return Ok(death);
        }
        // Its own executable, on the same tick: replaced, the core ends it (`Death::Replaced`).
        if let Some(change) = conns.scope(|frame| own.look(cx, frame)) {
            out.line(cx, format_args!("journal helper: its executable {change}"))?;
            let replaced = core.binary_replaced().map_err(HelperError::Core)?;
            if let Some(death) = driver.steps(cx, core, replaced)? {
                return Ok(death);
            }
        }
        let accepted = {
            let cx = cx.span("journal helper: waiting for an ask (a tick a second)");
            listener.accept(&cx.until(driver.clock.after(TICK)))
        };
        let conn = match accepted {
            Ok(conn) => conn,
            // The tick's own deadline: no ask this second. Only that: a stall, a cancel or an
            // arena out of room ends the helper (rule 3). `continue` on any `Stopped` could
            // turn a stall or an arena out of room (each returned before the tick's second is
            // up) into a loop with no wait in it, a tick's work again and again until the idle
            // expiry 30 minutes later (rule 18); a cancel ended at the next turn's take, said
            // as the stream failing, which it had not. A connection refused by its
            // peer's uid never comes here: every driver's accept skips it (blaze-core net.rs
            // `peer_is`).
            Err(error) if matches!(error.kind, NetKind::Stopped(Stopped::Deadline { .. })) => continue,
            Err(error) => return Err(HelperError::Accept(error).into()),
        };
        driver.asks += 1;
        let id = AskId(driver.asks);
        let began = driver.clock.now();
        let answered = conns.scope(|frame| driver.answer(cx, core, &conn, id, frame));
        let finished = conn.finish(&cx.until(driver.clock.after(CONN_WAIT)));
        let took = driver.clock.now().since(began).map_or(0, as_ns);
        let Answered { said, death } = answered?;
        let flush_ms = driver.last_flush_ns as f64 / 1e6;
        match finished {
            Ok(()) => out.line(cx, format_args!("journal helper: ask {}: {said}; {:.2} ms (its flush {flush_ms:.2} ms)", id.0, took as f64 / 1e6))?,
            Err(error) => out.line(cx, format_args!("journal helper: ask {}: {said}; its hang-up failed: {error}", id.0))?,
        }
        if let Some(death) = death {
            return Ok(death);
        }
    }
}

/// What one connection came to.
struct Answered {
    said: Said,
    death: Option<Death>,
}

/// One connection's line: what was answered, or how the connection failed.
enum Said {
    Answered { vouched: bool, names: usize },
    Failed(ConnFault),
    /// The core gave the answer, but the frame did not reach the command.
    Unsent { vouched: bool, error: NetError },
}

enum ConnFault {
    Hello(NetError),
    Ask(ReadFail),
    AskForm(helper::WireError),
}

impl fmt::Display for Said {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = |vouched: bool| if vouched { "vouched" } else { "did not vouch" };
        match self {
            Said::Answered { vouched, names } => write!(f, "{}, {names} name(s)", kind(*vouched)),
            Said::Failed(ConnFault::Hello(error)) => write!(f, "its hello not sent: {error}"),
            Said::Failed(ConnFault::Ask(error)) => write!(f, "its ask not read: {error}"),
            Said::Failed(ConnFault::AskForm(error)) => write!(f, "its ask did not parse: {error}"),
            Said::Unsent { vouched, error } => write!(f, "{}, but its answer was not sent: {error}", kind(*vouched)),
        }
    }
}

/// The driver's state beside the core: the stream and the clock.
struct Driver<'h, 'd> {
    held: &'h mut HeldChanges<'d, ReadWrite>,
    clock: &'h Clock,
    born: Instant,
    asks: u64,
    /// The last flush's time, for the ask's line.
    last_flush_ns: u64,
}

impl Driver<'_, '_> {
    /// The core's time: since the helper's start.
    fn now(&self) -> Nanos {
        nanos(self.clock, self.born)
    }

    /// What the stream told since the last take, told to the core; a failed take ends it.
    fn take(&mut self, cx: &Cx, core: &mut Helper<&Frame>) -> Result<Step, CliError> {
        let cx = cx.span("journal helper: what the stream told, taken");
        match self.held.take(&cx) {
            Ok(changes) => heard(core, &changes),
            Err(_) => Ok(core.failed(Fault::WatchFailed).map_err(HelperError::Core)?),
        }
    }

    /// The core's `step` carried out: a flush while it asks one (each completed with what it
    /// told and its journal place), and its death when it says.
    fn steps(&mut self, cx: &Cx, core: &mut Helper<&Frame>, step: Step) -> Result<Option<Death>, CliError> {
        let mut step = step;
        for _ in 0..FLUSHES_MOST {
            if let Some(death) = step.exit {
                return Ok(Some(death));
            }
            if !step.flush {
                return Ok(None);
            }
            step = self.flush(cx, core)?;
        }
        Err(HelperError::Loop { asks: self.asks }.into())
    }

    /// One flush of the held stream: what came before its cookie heard, then the place.
    fn flush(&mut self, cx: &Cx, core: &mut Helper<&Frame>) -> Result<Step, CliError> {
        let began = self.clock.now();
        let flushed = self.held.flush(&cx.until(self.clock.after(FLUSH_WAIT)));
        self.last_flush_ns = self.clock.now().since(began).map_or(0, as_ns);
        let changes = match flushed {
            Ok(changes) => changes,
            // No barrier, no place: nothing can be vouched for from here (rule 3).
            Err(_) => return Ok(core.failed(Fault::WatchFailed).map_err(HelperError::Core)?),
        };
        let step = heard(core, &changes)?;
        if step.exit.is_some() {
            return Ok(step);
        }
        let journal = JournalAt::of(&self.held.cursor().to_bytes()).expect("a cursor's bytes fit a journal place (the const assert above)");
        let placed = core.flushed(journal).map_err(HelperError::Core)?;
        Ok(Step { flush: step.flush || placed.flush, exit: placed.exit })
    }

    /// One connection: hello, the ask, the core's steps, the answer. A connection's failure is
    /// its line, never the helper's end; the core's and the stream's are.
    fn answer(&mut self, cx: &Cx, core: &mut Helper<&Frame>, conn: &Conn, id: AskId, frame: &Frame) -> Result<Answered, CliError> {
        let cx = cx.span("journal helper: one ask (hello, the ask read, a flush, the answer)");
        let net = cx.until(self.clock.after(CONN_WAIT));
        let mut bytes = Vec::new_in(frame);
        core.hello().encode(&mut bytes);
        if let Err(error) = conn.send_all(&net, &bytes) {
            return Ok(Answered { said: Said::Failed(ConnFault::Hello(error)), death: None });
        }
        let mut buf = Bytes::new_in(frame, "the journal helper's ask");
        let ask = match read_frame(&net, conn, &mut buf) {
            Ok(body) => match Ask::decode(body) {
                Ok(ask) => ask,
                Err(error) => return Ok(Answered { said: Said::Failed(ConnFault::AskForm(error)), death: None }),
            },
            Err(error) => return Ok(Answered { said: Said::Failed(ConnFault::Ask(error)), death: None }),
        };
        let step = core.asked(id, &ask, nanos(self.clock, self.born)).map_err(HelperError::Core)?;
        let death = self.steps(&cx, core, step)?;
        // One connection at a time: the answer taken is this ask's (any other is a bug).
        let mut answer = None;
        while let Some((of, told)) = core.take_answer() {
            if of != id {
                return Err(HelperError::Loop { asks: self.asks }.into());
            }
            answer = Some(told);
        }
        let Some(answer) = answer else {
            return Err(HelperError::Loop { asks: self.asks }.into());
        };
        let mut names: Vec<&str, &Frame> = Vec::new_in(frame);
        let vouched = match answer {
            Answer::Vouched { since, .. } => {
                names.extend(core.names_since(since));
                names.sort_unstable();
                names.dedup();
                true
            }
            Answer::CannotVouch { .. } => false,
        };
        bytes.clear();
        helper::encode_answer(&mut bytes, answer, core.journal(), &names);
        let said = match conn.send_all(&net, &bytes) {
            Ok(()) => Said::Answered { vouched, names: names.len() },
            Err(error) => Said::Unsent { vouched, error },
        };
        Ok(Answered { said, death })
    }
}

/// A batch told to the core (the module's doc, "What it is told"): its names but the
/// checkout's own entry, each lost subtree by its folder, a loss of the whole as its fault.
fn heard(core: &mut Helper<&Frame>, changes: &Changes) -> Result<Step, CliError> {
    if let Some(whole) = changes.lost().find(|(name, _)| name.is_empty()).map(|(_, lost)| lost) {
        let step = match whole {
            Lost::RootChanged => core.checkout_gone(),
            Lost::Dropped | Lost::Rescan | Lost::Unnamed => core.failed(Fault::Overflowed),
            Lost::Reset | Lost::Truncated | Lost::NoJournal => core.failed(Fault::WatchFailed),
        };
        return Ok(step.map_err(HelperError::Core)?);
    }
    let names = changes.files().filter(|name| !name.is_empty());
    let subtrees = changes.lost().map(|(name, _)| name).filter(|name| !name.is_empty());
    Ok(core.heard(names.chain(subtrees), false).map_err(HelperError::Core)?)
}

/// The helper's own executable, looked at on each tick (rule 70: a helper outlives no install
/// of its build; blaze-watch helper.rs `binary_replaced`, its plant `SurvivesReplace`). Each
/// tick takes the file's stamp (inode, device, times, length: one `stat` of one name, about a
/// microsecond); only a moved stamp reads the file's head for its linker id (bstd
/// `build_id_in`, the first [`crate::roster::EXE_HEAD`] bytes, as the roster reads them). The
/// same build at a new inode (reinstalled whole) is no replacement: the stamp is kept and the
/// helper goes on. A file gone, of another build, or one whose id cannot be read, is: the
/// command of the new build is refused by this helper's hello anyway (`OtherVersion`), so the
/// tick only frees the checkout's stream and socket a second after the install, not an ask
/// later.
pub(crate) struct OwnExe {
    folder: Dir<ReadWrite>,
    leaf: Path,
    build: BuildId,
    stamp: Stamp,
}

/// Why its executable could not be looked at, at the helper's start.
#[derive(Debug)]
pub(crate) enum OwnExeWhy {
    /// The host named a path that is not absolute, or one directly in `/`.
    NotAPath,
    Path(PathError),
    Home(HomeError),
    Fs(FsError),
    /// No file at the path this process was started from.
    Gone,
}

impl fmt::Display for OwnExeWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnExeWhy::NotAPath => f.write_str("not an absolute path to a file in a folder"),
            OwnExeWhy::Path(error) => write!(f, "{error}"),
            OwnExeWhy::Home(error) => write!(f, "its folder: {error}"),
            OwnExeWhy::Fs(error) => write!(f, "{error}"),
            OwnExeWhy::Gone => f.write_str("no file there"),
        }
    }
}

/// Why the executable at its path is no longer the build this helper runs.
pub(crate) enum ExeChange {
    Gone,
    OtherBuild { now: BuildId },
    /// A file whose head carries no linker id this reader knows.
    NoBuildId,
    Unreadable(FsError),
}

impl fmt::Display for ExeChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExeChange::Gone => f.write_str("is gone from its path: the helper ends"),
            ExeChange::OtherBuild { now } => write!(f, "is another build now ({now:?}): the helper ends"),
            ExeChange::NoBuildId => f.write_str("carries no linker id it can read now: the helper ends"),
            ExeChange::Unreadable(error) => write!(f, "could not be read: {error}: the helper ends"),
        }
    }
}

/// What one look found (sans-IO: [`look_at`]).
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Looked {
    /// Its stamp is the one kept.
    Same,
    /// Its stamp moved, its build did not: the new stamp is kept.
    Moved(Stamp),
    /// It is no longer this build.
    Replaced,
}

/// A planted bug for [`look_at`]'s test: a moved stamp taken as the same file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LookPlant {
    None,
    StampOnly,
}

/// The decision of one look: `was` the stamp kept, `now` the file's stamp (`None`: no file),
/// `read` its head's build id, called only when the stamp moved.
pub(crate) fn look_at<E>(own: BuildId, was: Stamp, now: Option<Stamp>, read: impl FnOnce() -> Result<Option<BuildId>, E>, plant: LookPlant) -> (Looked, Option<Result<Option<BuildId>, E>>) {
    let Some(now) = now else { return (Looked::Replaced, None) };
    if now == was || plant == LookPlant::StampOnly {
        return (Looked::Same, None);
    }
    let read = read();
    let looked = match &read {
        Ok(Some(id)) if *id == own => Looked::Moved(now),
        _ => Looked::Replaced,
    };
    (looked, Some(read))
}

impl OwnExe {
    /// The executable at `exe` (absolute, from `Host::exe`), this process's build `build`:
    /// its folder opened as a second tree (`Fs::home`, as the roster does) and its stamp taken.
    pub(crate) fn open(cx: &Cx, fs: &Fs, exe: &str, build: BuildId) -> Result<OwnExe, OwnExeWhy> {
        let (folder, leaf) = exe.strip_prefix('/').and_then(|rest| rest.rsplit_once('/')).ok_or(OwnExeWhy::NotAPath)?;
        let folder = Path::new(folder).map_err(OwnExeWhy::Path)?;
        let leaf = Path::new(leaf).map_err(OwnExeWhy::Path)?;
        let folder = fs.home(cx, "/", &folder).map_err(OwnExeWhy::Home)?;
        let stamp = folder.stat(cx, &leaf).map_err(OwnExeWhy::Fs)?.ok_or(OwnExeWhy::Gone)?.stamp;
        Ok(OwnExe { folder, leaf, build, stamp })
    }

    /// One look ([`look_at`]): `None` while it is this build (a moved stamp kept), else why not.
    pub(crate) fn look(&mut self, cx: &Cx, frame: &Frame) -> Option<ExeChange> {
        let cx = cx.span("journal helper: its own executable looked at (a stat; its head read when the stat moved)");
        let now = match self.folder.stat(&cx, &self.leaf) {
            Ok(meta) => meta.map(|meta| meta.stamp),
            Err(error) => return Some(ExeChange::Unreadable(error)),
        };
        let read = || -> Result<Option<BuildId>, FsError> {
            let file = self.folder.open_read(&cx, &self.leaf)?;
            let mut head = Bytes::new_in(frame, "the journal helper's executable's head");
            file.read_at(&cx, 0, crate::roster::EXE_HEAD, &mut head)?;
            Ok(build_id_in(head.as_slice()))
        };
        match look_at(self.build, self.stamp, now, read, LookPlant::None) {
            (Looked::Same, _) => None,
            (Looked::Moved(stamp), _) => {
                self.stamp = stamp;
                None
            }
            (Looked::Replaced, None) => Some(ExeChange::Gone),
            (Looked::Replaced, Some(Ok(Some(now)))) => Some(ExeChange::OtherBuild { now }),
            (Looked::Replaced, Some(Ok(None))) => Some(ExeChange::NoBuildId),
            (Looked::Replaced, Some(Err(error))) => Some(ExeChange::Unreadable(error)),
        }
    }
}

fn nanos(clock: &Clock, born: Instant) -> Nanos {
    Nanos(clock.now().since(born).map_or(0, as_ns))
}

fn as_ns(took: Duration) -> u64 {
    u64::try_from(took.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "tests/journal_helper_tests.rs"]
mod tests;
