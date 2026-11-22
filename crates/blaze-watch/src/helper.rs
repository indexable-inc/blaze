//! The journal helper's session and wire (end-state.md D168 mechanism 4; rule 70): the process
//! a command may leave behind for one checkout, which holds that checkout's file-event stream
//! open and answers "what changed since place P, and the place now" over a Unix socket, so a
//! command pays a socket round trip and one barrier instead of making, replaying, flushing and
//! stopping streams (tempo, score-c1725wave42: "journal replayed" 99 ms on an edit, 141 ms on a
//! no-op, of which the streams' setup is nearly all).
//!
//! **What it is made of.** The answer's logic is [`crate::since::Watcher`], unchanged: places
//! issued at a flush's completion, names kept with the newest place's tag, one flush per ask
//! asked after the ask came, memory fixed when made, a forgotten place refused by name
//! ([`CannotVouch::Forgotten`]: "lost: sweep"), expiry by the times passed in. This module adds
//! what a process serving commands needs around it, sans-IO (rule 14):
//!
//! - the hello ([`Hello`]): the helper's version (the build id of its binary), its instance
//!   and the checkout it watches, the first frame on every connection;
//! - the ask ([`Ask`]): the command's version and its saved place. An ask of another version is
//!   refused with [`Death::Replaced`] and the helper ends: what it holds is only what the
//!   journal rebuilds, so the command's own build starts its own ([`Helper::asked`]). Its file
//!   replaced is the same death ([`Helper::binary_replaced`]), from the driver's check of its
//!   executable's identity at each ask and each tick;
//! - the checkout gone: [`Helper::checkout_gone`], the watch's root-changed fault;
//! - the command's side ([`Asking`]): connect, hello checked, ask, answer, each step under one
//!   deadline ([`ASK_DEADLINE`]); a missing socket, a stale one (refused: no process behind it),
//!   another checkout, a frame that does not parse and a passed deadline are each a typed
//!   [`NotAsked`], and the command replays the journal itself (journal_sweep.rs: the seam's
//!   other instantiation, `Replay`). That is the slot's other version, not a fallback (rule 3):
//!   both are complete answers to one question, the profile names which answered and why the
//!   other did not, and the simulation holds both to one oracle (rule 78).
//!
//! **The place's guarantee, with a live stream.** A place is issued when a flush completes, and
//! a flush on the live FSEvents stream is the cookie barrier the replay already uses (bstd
//! fs/journal.rs, "Why the flush"): the helper writes a cookie file under the checkout's
//! `.blaze` and the flush completes when the stream tells it, so every change made before the
//! ask came is heard before its answer. Its expected cost is the cookie's trip through
//! fseventsd on a stream made with latency 0 and `NoDefer`: not measured yet; the command's
//! `journal:` line prints the ask's whole time and the helper's barrier time beside it, and an
//! ask over [`ASK_DEADLINE`] is refused by its deadline, never waited out.
//!
//! **The socket.** [`socket_path`] under blaze's folder in the home, named by the checkout's
//! key, in [`SOCKET_FOLDER`], which the helper
//! before it listens and each command before it connects make the user's alone (mode 0700,
//! owned by the user, nothing in it another may write: bstd's private-folder check, the
//! store's): only this user can make an entry there, so a socket found there is this user's,
//! and only this user can reach it to connect (watch.md, "Security"). The socket is 0600 before
//! its name appears (blaze-core net.rs). The peer's uid read on accept (`getpeereid`) would be
//! a second wall; blaze-core's `Done::Accepted` carries none yet. One listener per checkout: a
//! helper that finds a live one there exits, one that finds a stale one replaces it. The
//! frames are [`crate::frame`]'s form: a `u32` LE length, then one deterministic CBOR array
//! whose first item is [`FORM`].
//!
//! **What it never does** (rule 70): build, check, read a source file, or hold anything a
//! command's records do not rebuild. It exits after [`IDLE`] without an ask, when its checkout
//! is gone, when its binary is replaced, or on any fault of its stream (the core's deaths).

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::Nanos;
use crate::cbor;
use crate::since::{self, Answer, AskId, CannotVouch, Death, Fault, Place, SinceError, Step, Watcher};

/// The wire's form: a frame of another form is refused whole, never read in part.
pub const FORM: u64 = 1;

/// The sockets' folder, relative to blaze's folder in the home (`<HOME>/.blaze`, blaze-cli
/// store.rs `HOME_FOLDER`), one for every checkout of the user: made the user's alone (mode
/// 0700, checked owned by the user with nothing in it another may write) by the helper before
/// it listens and by a command before it connects, so a socket found there was made by this
/// user. Under the home, not the checkout (since 2026-11-21): the path's length is the home's
/// plus a constant, never the checkout's depth, so no checkout is too deep for a helper.
pub const SOCKET_FOLDER: &str = "v2/journal";

/// Bytes of a socket's name in [`SOCKET_FOLDER`] ([`socket_path`]): 16 hex digits, `.sock`.
pub const SOCKET_NAME_BYTES: usize = 16 + ".sock".len();

/// Bytes of [`socket_path`]: the folder, a `/`, the name.
pub const SOCKET_PATH_BYTES: usize = SOCKET_FOLDER.len() + 1 + SOCKET_NAME_BYTES;

/// The checkout's socket, relative to blaze's folder in the home: `v2/journal/<the first 8
/// bytes of its key in hex>.sock`, mode 0600 (blaze-core net.rs makes it so before its name
/// appears). Named by the key, never by a path (no absolute path in a name): two checkouts
/// whose keys share their first 8 bytes (2^-64 a pair) meet at one socket, where the hello's
/// whole key refuses the other's helper ([`NotAsked::OtherCheckout`]), never trusts it.
pub fn socket_path(key: &CheckoutKey) -> [u8; SOCKET_PATH_BYTES] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; SOCKET_PATH_BYTES];
    let folder = SOCKET_FOLDER.as_bytes();
    out[..folder.len()].copy_from_slice(folder);
    out[folder.len()] = b'/';
    let name = &mut out[folder.len() + 1..];
    for (at, byte) in key.0[..8].iter().enumerate() {
        name[2 * at] = HEX[usize::from(byte >> 4)];
        name[2 * at + 1] = HEX[usize::from(byte & 0xf)];
    }
    name[16..].copy_from_slice(b".sock");
    out
}

/// How long a command waits for the whole ask (connect, hello, ask, answer): a helper that
/// has not answered by then is not waited for (rule 18). Ten times the barrier's expected
/// trip, which the profile prints until it is measured.
pub const ASK_DEADLINE: Nanos = Nanos::millis(50);

/// A helper exits this long after its last ask (rule 70).
pub const IDLE: Nanos = Nanos::secs(30 * 60);

/// The most names one answer carries (the core's own cap, [`since::Config::new`]).
pub const MOST_NAMES: usize = 1 << 16;

/// The most bytes of one frame: the names' cap, their heads and the fixed fields.
pub const MOST_FRAME: usize = (8 << 20) + (MOST_NAMES * 4) + 256;

const HELLO: u64 = 0;
const ASK: u64 = 1;
const VOUCHED: u64 = 2;
const REFUSED: u64 = 3;

/// A blaze binary's version: its build id (the Mach-O `LC_UUID`, the ELF build-id note), at
/// most 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Version {
    bytes: [u8; 32],
    len: u8,
}

impl Version {
    /// `bytes` as a version: `None` when empty or longer than 32 bytes.
    pub fn of(bytes: &[u8]) -> Option<Version> {
        if bytes.is_empty() || bytes.len() > 32 {
            return None;
        }
        let mut out = [0; 32];
        out[..bytes.len()].copy_from_slice(bytes);
        Some(Version { bytes: out, len: u8::try_from(bytes.len()).ok()? })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

/// A place in the system's journal of changes as the driver's cursor encodes it (bstd
/// `Cursor::to_bytes`, 25 bytes), carried opaque: the place of the flush that issued a helper's
/// place (its cookie's event), which a command saves beside the helper's so a helper's death
/// costs it one replay from there, never a sweep of every row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JournalAt {
    bytes: [u8; JournalAt::MOST],
    len: u8,
}

impl JournalAt {
    /// The most bytes of one.
    pub const MOST: usize = 32;

    /// No place yet: before the first flush.
    pub const NONE: JournalAt = JournalAt { bytes: [0; JournalAt::MOST], len: 0 };

    /// `bytes` as a place: `None` when longer than [`JournalAt::MOST`].
    pub fn of(bytes: &[u8]) -> Option<JournalAt> {
        let mut out = [0; JournalAt::MOST];
        out.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(JournalAt { bytes: out, len: u8::try_from(bytes.len()).ok()? })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

/// The checkout a helper watches: the digest of its name as the helper's and the command's
/// drivers both reach it, so a socket of another checkout is refused by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckoutKey(pub [u8; 32]);

/// The helper's first frame on a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hello {
    pub version: Version,
    pub instance: u64,
    pub checkout: CheckoutKey,
}

/// A command's ask.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ask {
    pub version: Version,
    /// The place the command's stored answer saved: `None` for none.
    pub since: Option<Place>,
}

/// A planted bug of the helper's own, for the simulation's controls (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HelperPlant {
    None,
    /// An ask of another version is answered as one of its own: the helper survives a binary
    /// change.
    SurvivesReplace,
}

/// The helper's session: the core, and the identity every hello says (the module's doc).
pub struct Helper<A: Allocator + Copy> {
    core: Watcher<A>,
    hello: Hello,
    /// The journal's place of the newest place issued.
    journal: JournalAt,
    /// The ask of another version that ended this helper, its refusal not yet taken.
    replaced_ask: Option<AskId>,
    plant: HelperPlant,
}

impl<A: Allocator + Copy> Helper<A> {
    /// A helper of binary `version` for `checkout`, its core's tables in `alloc` (once), made
    /// with its first flush out ([`Watcher::new_in`]).
    pub fn new_in(config: since::Config, version: Version, checkout: CheckoutKey, instance: u64, now: Nanos, alloc: A) -> Result<Helper<A>, SinceError> {
        let core = Watcher::new_in(config, instance, now, alloc)?;
        Ok(Helper { core, hello: Hello { version, instance, checkout }, journal: JournalAt::NONE, replaced_ask: None, plant: HelperPlant::None })
    }

    /// As [`Helper::new_in`], with the core's and the session's plants.
    pub(crate) fn planted_in(
        config: since::Config, hello: Hello, now: Nanos, alloc: A, core_plant: since::SincePlant, plant: HelperPlant,
    ) -> Result<Helper<A>, SinceError> {
        let core = Watcher::planted_in(config, hello.instance, now, alloc, core_plant)?;
        Ok(Helper { core, hello, journal: JournalAt::NONE, replaced_ask: None, plant })
    }

    /// What every connection is told first.
    pub fn hello(&self) -> Hello {
        self.hello
    }

    /// A command asks: of another version, the helper ends ([`Death::Replaced`]) and the ask
    /// is refused for it; else the core's [`Watcher::asked`].
    pub fn asked(&mut self, id: AskId, ask: &Ask, now: Nanos) -> Result<Step, SinceError> {
        if ask.version != self.hello.version && self.plant != HelperPlant::SurvivesReplace {
            // The core refuses the asks it holds; this one is refused here, so the core's
            // answers' room (its `most_asks`) never has to hold one more.
            let step = self.core.replaced()?;
            self.replaced_ask = Some(id);
            return Ok(step);
        }
        self.core.asked(id, ask.since, now)
    }

    /// The driver found its own executable replaced (another inode or build id at its path).
    pub fn binary_replaced(&mut self) -> Result<Step, SinceError> {
        match self.plant {
            HelperPlant::SurvivesReplace => Ok(Step::default()),
            HelperPlant::None => self.core.replaced(),
        }
    }

    /// The checkout's folder is gone or moved: the watch's root changed.
    pub fn checkout_gone(&mut self) -> Result<Step, SinceError> {
        self.core.failed(Fault::RootChanged)
    }

    /// The watch told `names` (relative to the checkout); `lost`: it dropped some.
    pub fn heard<'n>(&mut self, names: impl IntoIterator<Item = &'n str>, lost: bool) -> Result<Step, SinceError> {
        self.core.heard(names, lost)
    }

    /// The flush asked last completed (its cookie was heard); `journal` is the cursor of the
    /// last batch the watch read, cookie's included (every change at or before it was told
    /// first, every one after it will be: the place and it are one moment). The
    /// answers it makes carry it: a command saves both places, so a helper's death costs the
    /// next command one replay from `journal`, never a sweep of every row.
    pub fn flushed(&mut self, journal: JournalAt) -> Result<Step, SinceError> {
        self.journal = journal;
        self.core.flushed()
    }

    /// The journal's place of the newest place issued ([`Helper::flushed`]); none before the
    /// first.
    pub fn journal(&self) -> JournalAt {
        self.journal
    }

    pub fn failed(&mut self, fault: Fault) -> Result<Step, SinceError> {
        self.core.failed(fault)
    }

    pub fn tick(&mut self, now: Nanos) -> Result<Step, SinceError> {
        self.core.tick(now)
    }

    pub fn take_answer(&mut self) -> Option<(AskId, Answer)> {
        if let Some(answer) = self.core.take_answer() {
            return Some(answer);
        }
        let id = self.replaced_ask.take()?;
        Some((id, Answer::CannotVouch { why: CannotVouch::Dead(Death::Replaced), place: None }))
    }

    pub fn names_since(&self, since: Place) -> impl Iterator<Item = &str> + '_ {
        self.core.names_since(since)
    }

    pub fn is_dead(&self) -> bool {
        self.core.is_dead()
    }
}

/// Why a command's ask got no answer from a helper: the command replays the journal itself,
/// and the profile says this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotAsked {
    /// No socket at [`socket_path`]: no helper was started, or it exited and removed it.
    NoSocket,
    /// A socket with no process behind it (the connect was refused): a helper that died.
    Stale,
    /// The hello names another checkout.
    OtherCheckout,
    /// The hello is of another build: that helper ends at this ask, and this command's build
    /// starts its own.
    OtherVersion,
    /// The deadline passed at this step.
    Deadline { at: AskStep },
    /// A frame that does not parse, or the connection closed mid-ask.
    Broken { at: AskStep },
}

impl fmt::Display for NotAsked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotAsked::NoSocket => f.write_str("no helper's socket"),
            NotAsked::Stale => f.write_str("a stale helper's socket (nothing listens)"),
            NotAsked::OtherCheckout => f.write_str("the helper's socket is of another checkout"),
            NotAsked::OtherVersion => f.write_str("the helper is of another build"),
            NotAsked::Deadline { at } => write!(f, "the helper did not answer within {} ms ({at})", ASK_DEADLINE.0 / 1_000_000),
            NotAsked::Broken { at } => write!(f, "the helper's connection broke ({at})"),
        }
    }
}

/// Where in an ask a command is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AskStep {
    Connect,
    Hello,
    Answer,
}

impl fmt::Display for AskStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AskStep::Connect => "connecting",
            AskStep::Hello => "waiting for its hello",
            AskStep::Answer => "waiting for its answer",
        })
    }
}

/// What the command's driver does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AskNext {
    /// Connect to [`socket_path`], by [`Asking::deadline`].
    Connect,
    /// Read a frame (the hello, then the answer), by the deadline.
    Read,
    /// Write this ask, then read.
    Send(Ask),
    /// Answered: the names (in the answer's frame) and the place to save.
    Answered(AskAnswer),
    /// No helper answered: replay the journal, and start a helper when `start` says.
    Replay { why: NotAsked, start: bool },
}

/// A helper's answer as the command takes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AskAnswer {
    /// `journal`: the journal's place of `place` ([`Helper::journal`]), saved beside it.
    Vouched { since: Place, place: Place, journal: JournalAt },
    /// The helper could not vouch: the command replays from the journal place it saved (every
    /// row statted when it saved none), and saves `place` where one was issued with ITS journal
    /// place `journal` (never the replay's end: the pair is one moment, so the next command's
    /// replay from it and the helper's names since it agree), and a helper is started when the
    /// refusal was its death.
    CannotVouch { why: CannotVouch, place: Option<Place>, journal: JournalAt },
}

/// The command's side of one ask (sans-IO): one deadline over every step.
#[derive(Clone, Copy, Debug)]
pub struct Asking {
    version: Version,
    checkout: CheckoutKey,
    since: Option<Place>,
    deadline: Nanos,
    step: AskStep,
    done: bool,
    /// The hello read: an answer's places must be of its instance, and its version this one.
    theirs: Option<Hello>,
}

/// A planted bug of the command's side, for the simulation's controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AskPlant {
    None,
    /// The deadline is not checked: a command waits on a helper that never answers.
    NoDeadline,
}

impl Asking {
    /// An ask from `since` by a command of `version` in `checkout`, begun at `now`: connect.
    pub fn new(version: Version, checkout: CheckoutKey, since: Option<Place>, now: Nanos) -> (Asking, AskNext) {
        let asking = Asking { version, checkout, since, deadline: now.after(ASK_DEADLINE), step: AskStep::Connect, done: false, theirs: None };
        (asking, AskNext::Connect)
    }

    /// The time every step of this ask ends by.
    pub fn deadline(&self) -> Nanos {
        self.deadline
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    fn end(&mut self, next: AskNext) -> AskNext {
        self.done = true;
        next
    }

    /// The connect's outcome: `Ok` connected; `Err(NotAsked::NoSocket | Stale)` refused.
    pub fn connected(&mut self, result: Result<(), NotAsked>) -> AskNext {
        match result {
            Ok(()) => {
                self.step = AskStep::Hello;
                AskNext::Read
            }
            Err(why) => self.end(AskNext::Replay { why, start: true }),
        }
    }

    /// The helper's hello was read.
    pub fn hello(&mut self, hello: &Hello) -> AskNext {
        if hello.checkout != self.checkout {
            return self.end(AskNext::Replay { why: NotAsked::OtherCheckout, start: false });
        }
        self.theirs = Some(*hello);
        self.step = AskStep::Answer;
        // Of another version, the ask is still sent: the helper ends at it ([`Helper::asked`]),
        // and its answer is refused here, so the next command's helper is this build's.
        AskNext::Send(Ask { version: self.version, since: self.since })
    }

    /// The helper's answer. Of a helper of another build it is not taken: that helper ended at
    /// this ask, and this command's build starts its own.
    pub fn answered(&mut self, answer: AskAnswer) -> AskNext {
        let Some(theirs) = self.theirs else {
            return self.end(AskNext::Replay { why: NotAsked::Broken { at: AskStep::Hello }, start: false });
        };
        if theirs.version != self.version {
            return self.end(AskNext::Replay { why: NotAsked::OtherVersion, start: true });
        }
        let ours = |place: Place| place.instance == theirs.instance;
        let answer = match answer {
            AskAnswer::Vouched { since, place, .. } if !ours(since) || !ours(place) => {
                return self.end(AskNext::Replay { why: NotAsked::Broken { at: AskStep::Answer }, start: false });
            }
            answer => answer,
        };
        self.end(AskNext::Answered(answer))
    }

    /// The connection broke, or a frame did not parse.
    pub fn broke(&mut self) -> AskNext {
        let at = self.step;
        self.end(AskNext::Replay { why: NotAsked::Broken { at }, start: false })
    }

    /// The time is `now`: past the deadline the ask is given up (rule 18).
    pub fn tick(&mut self, now: Nanos) -> Option<AskNext> {
        self.tick_planted(now, AskPlant::None)
    }

    pub(crate) fn tick_planted(&mut self, now: Nanos, plant: AskPlant) -> Option<AskNext> {
        if self.done || now < self.deadline || plant == AskPlant::NoDeadline {
            return None;
        }
        let at = self.step;
        Some(self.end(AskNext::Replay { why: NotAsked::Deadline { at }, start: false }))
    }
}

/// Why a frame was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    Cbor(cbor::Error),
    /// The frame's form is not [`FORM`].
    Form(u64),
    /// Its kind is not the one expected here.
    Kind(u64),
    /// A version of no bytes or more than 32, a checkout key not 32 bytes, a refusal's code of
    /// none, a name that is not a relative path.
    Field(&'static str),
    /// The length word says more than [`MOST_FRAME`], or less than the frame holds.
    Length(u64),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Cbor(error) => write!(f, "the journal helper's frame: {error}"),
            WireError::Form(form) => write!(f, "the journal helper's frame is of form {form}, not {FORM}"),
            WireError::Kind(kind) => write!(f, "the journal helper's frame is of kind {kind}, not the one expected here"),
            WireError::Field(what) => write!(f, "the journal helper's frame: {what}"),
            WireError::Length(len) => write!(f, "the journal helper's frame says {len} bytes (at most {MOST_FRAME})"),
        }
    }
}

impl From<cbor::Error> for WireError {
    fn from(error: cbor::Error) -> WireError {
        WireError::Cbor(error)
    }
}

/// The frame's length word, then its body, appended: `body` writes the CBOR array.
fn framed<A: Allocator>(out: &mut Vec<u8, A>, body: impl FnOnce(&mut Vec<u8, A>)) {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    body(out);
    let len = u32::try_from(out.len() - at - 4).unwrap_or(u32::MAX);
    out[at..at + 4].copy_from_slice(&len.to_le_bytes());
}

/// The length a frame's first four bytes say: `None` for fewer than four, refused past
/// [`MOST_FRAME`].
pub fn frame_len(head: &[u8]) -> Option<Result<usize, WireError>> {
    let word = u32::from_le_bytes(head.get(..4)?.try_into().ok()?);
    let len = usize::try_from(word).unwrap_or(usize::MAX);
    Some(if len > MOST_FRAME { Err(WireError::Length(u64::from(word))) } else { Ok(len) })
}

fn head(reader: &mut cbor::Reader<'_>, items: usize, kind: u64) -> Result<(), WireError> {
    reader.array_of(items)?;
    let form = reader.uint()?;
    if form != FORM {
        return Err(WireError::Form(form));
    }
    let got = reader.uint()?;
    if got != kind {
        return Err(WireError::Kind(got));
    }
    Ok(())
}

fn version_of(reader: &mut cbor::Reader<'_>) -> Result<Version, WireError> {
    Version::of(reader.bytes(32, "a version")?).ok_or(WireError::Field("a version of no bytes"))
}

impl Hello {
    /// `[FORM, 0, version, instance, checkout]`, framed.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        framed(out, |out| {
            cbor::array(out, 5);
            cbor::uint(out, FORM);
            cbor::uint(out, HELLO);
            cbor::bytes(out, self.version.as_bytes());
            cbor::uint(out, self.instance);
            cbor::bytes(out, &self.checkout.0);
        });
    }

    /// Of a frame's body (after its length word).
    pub fn decode(body: &[u8]) -> Result<Hello, WireError> {
        let mut reader = cbor::Reader::new(body);
        head(&mut reader, 5, HELLO)?;
        let version = version_of(&mut reader)?;
        let instance = reader.uint()?;
        let checkout = CheckoutKey(reader.fixed::<32>()?);
        reader.end()?;
        Ok(Hello { version, instance, checkout })
    }
}

impl Ask {
    /// `[FORM, 1, version, instance, seq]` (seq 0: no place), framed.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        let place = self.since.unwrap_or(Place { instance: 0, seq: 0 });
        framed(out, |out| {
            cbor::array(out, 5);
            cbor::uint(out, FORM);
            cbor::uint(out, ASK);
            cbor::bytes(out, self.version.as_bytes());
            cbor::uint(out, place.instance);
            cbor::uint(out, place.seq);
        });
    }

    pub fn decode(body: &[u8]) -> Result<Ask, WireError> {
        let mut reader = cbor::Reader::new(body);
        head(&mut reader, 5, ASK)?;
        let version = version_of(&mut reader)?;
        let instance = reader.uint()?;
        let seq = reader.uint()?;
        reader.end()?;
        Ok(Ask { version, since: (seq > 0).then_some(Place { instance, seq }) })
    }
}

/// A refusal's code and two details on the wire: one row per [`CannotVouch`] a helper sends
/// ([`CannotVouch::NoHelper`] is the command's own, never sent).
fn refusal_code(why: CannotVouch) -> (u64, u64, u64) {
    match why {
        CannotVouch::NoHelper => (0, 0, 0),
        CannotVouch::NoPlace => (1, 0, 0),
        CannotVouch::OtherInstance { asked, here } => (2, asked, here),
        CannotVouch::NotIssued { seq, newest } => (3, seq, newest),
        CannotVouch::Forgotten { seq, from } => (4, seq, from),
        CannotVouch::TooManyAsks { most } => (5, u64::try_from(most).unwrap_or(u64::MAX), 0),
        CannotVouch::Dead(Death::Expired) => (6, 0, 0),
        CannotVouch::Dead(Death::Replaced) => (7, 0, 0),
        CannotVouch::Dead(Death::Fault(Fault::Overflowed)) => (8, 0, 0),
        CannotVouch::Dead(Death::Fault(Fault::RootChanged)) => (9, 0, 0),
        CannotVouch::Dead(Death::Fault(Fault::WatchRefused { watched, limit })) => (10, watched, limit.map_or(0, |it| it.saturating_add(1))),
        CannotVouch::Dead(Death::Fault(Fault::WatchFailed)) => (11, 0, 0),
    }
}

fn refusal_of(code: u64, a: u64, b: u64) -> Result<CannotVouch, WireError> {
    Ok(match code {
        1 => CannotVouch::NoPlace,
        2 => CannotVouch::OtherInstance { asked: a, here: b },
        3 => CannotVouch::NotIssued { seq: a, newest: b },
        4 => CannotVouch::Forgotten { seq: a, from: b },
        5 => CannotVouch::TooManyAsks { most: usize::try_from(a).unwrap_or(usize::MAX) },
        6 => CannotVouch::Dead(Death::Expired),
        7 => CannotVouch::Dead(Death::Replaced),
        8 => CannotVouch::Dead(Death::Fault(Fault::Overflowed)),
        9 => CannotVouch::Dead(Death::Fault(Fault::RootChanged)),
        10 => CannotVouch::Dead(Death::Fault(Fault::WatchRefused { watched: a, limit: b.checked_sub(1) })),
        11 => CannotVouch::Dead(Death::Fault(Fault::WatchFailed)),
        _ => return Err(WireError::Field("a refusal's code of none")),
    })
}

/// An answer, framed: `[FORM, 2, instance, since, place, journal, [name...]]` when vouched
/// (each name once, sorted: the reader takes them as bstd `Changes::files` are; `journal` the
/// place's [`Helper::journal`], its bytes), else `[FORM, 3, code, a, b, instance, place,
/// journal]` (place 0: none issued, and `journal` empty).
pub fn encode_answer<'n, A: Allocator>(out: &mut Vec<u8, A>, answer: Answer, journal: JournalAt, names: &[&'n str]) {
    framed(out, |out| match answer {
        Answer::Vouched { since, place } => {
            cbor::array(out, 7);
            cbor::uint(out, FORM);
            cbor::uint(out, VOUCHED);
            cbor::uint(out, place.instance);
            cbor::uint(out, since.seq);
            cbor::uint(out, place.seq);
            cbor::bytes(out, journal.as_bytes());
            cbor::array(out, u64::try_from(names.len()).unwrap_or(u64::MAX));
            for name in names {
                cbor::text(out, name);
            }
        }
        Answer::CannotVouch { why, place } => {
            let (code, a, b) = refusal_code(why);
            let (place, journal) = match place {
                Some(place) => (place, journal),
                None => (Place { instance: 0, seq: 0 }, JournalAt::NONE),
            };
            cbor::array(out, 8);
            cbor::uint(out, FORM);
            cbor::uint(out, REFUSED);
            cbor::uint(out, code);
            cbor::uint(out, a);
            cbor::uint(out, b);
            cbor::uint(out, place.instance);
            cbor::uint(out, place.seq);
            cbor::bytes(out, journal.as_bytes());
        }
    });
}

/// An answer's frame body read: the answer, and its names pushed onto `names` (in place).
pub fn decode_answer<'b, A: Allocator>(body: &'b [u8], names: &mut Vec<&'b str, A>) -> Result<AskAnswer, WireError> {
    let mut reader = cbor::Reader::new(body);
    let items = reader.array(8, "an answer's fields")?;
    let form = reader.uint()?;
    if form != FORM {
        return Err(WireError::Form(form));
    }
    let answer = match (reader.uint()?, items) {
        (VOUCHED, 7) => {
            let instance = reader.uint()?;
            let since = reader.uint()?;
            let place = reader.uint()?;
            let journal = JournalAt::of(reader.bytes(JournalAt::MOST, "a journal's place")?).ok_or(WireError::Field("a journal's place over 32 bytes"))?;
            let count = reader.array(MOST_NAMES, "an answer's names")?;
            for _ in 0..count {
                let name = reader.text(since::MOST_NAME, "a name told")?;
                if name.is_empty() || name.starts_with('/') || name.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
                    return Err(WireError::Field("a name told that is not a relative path"));
                }
                names.push(name);
            }
            AskAnswer::Vouched { since: Place { instance, seq: since }, place: Place { instance, seq: place }, journal }
        }
        (REFUSED, 8) => {
            let why = refusal_of(reader.uint()?, reader.uint()?, reader.uint()?)?;
            let instance = reader.uint()?;
            let seq = reader.uint()?;
            let journal = JournalAt::of(reader.bytes(JournalAt::MOST, "a journal's place")?).ok_or(WireError::Field("a journal's place over 32 bytes"))?;
            AskAnswer::CannotVouch { why, place: (seq > 0).then_some(Place { instance, seq }), journal }
        }
        (kind, _) => return Err(WireError::Kind(kind)),
    };
    reader.end()?;
    Ok(answer)
}
