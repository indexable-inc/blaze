//! What one operating-system call a test made means for its result (docs/v2/plan/design/test.md,
//! "End state", piece 11; test-selection.md section 2): an ASK (an outside read, whose identity
//! is digested now and again before the next run), the test's OWN business (its temporary
//! folder, its own loopback port, `/dev/null`), NOT KEPT by a named reason (the real clock, a
//! peer outside the test, code the tracer cannot see, a descriptor it did not see opened), or
//! REFUSED (a write outside its temporary folder, a connection off the machine: the sandbox
//! refuses these and the test fails naming the thing).
//!
//! [`classify`] is pure and table-driven: an [`Op`] is reduced to its [`Kind`] and [`Site`], and
//! [`RULES`] holds one row for every pair. The wrappers that OBSERVE calls inside a forked child
//! are the launcher's (they bind the program's imports of `open`, `stat`, `readdir`, `getenv`,
//! `clock_gettime`, `connect`, `bind`, `getrandom` and `posix_spawn` to recording wrappers: a
//! later piece); each calls [`Observe::saw`] before the real call, with the path already
//! resolved against the workspace root and the test's temporary folder (no `..` left: the
//! wrapper resolves it), and does what the [`Allowed`] it gets back says.
//!
//! Two choices, each a reading of the design:
//!
//! - Randomness (`getrandom`, `/dev/urandom`) is the test's own: std's `RandomState` reads it in
//!   nearly every program, so marking it would keep nothing; a result that rests on it shows up
//!   as a `Flaky` finding at one address (result.rs), which is the answer the design gives.
//! - A monotonic clock read is the real clock as much as a wall-clock read: what a test does
//!   with an `Instant` (a timeout, a measured wait) depends on the machine's speed. Both are
//!   [`NotKept::Clock`]. A `#[bstd::test]` gets the virtual clock and never reaches here.

#![blaze::sans_io]

use crate::plan::NotKept;
use crate::result::Refusal;

/// Where a path is, as the wrapper resolved it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Where<'p> {
    /// Under the workspace root: the path from the root, `/`-separated.
    Root(&'p str),
    /// Under the test's own temporary folder.
    Temp,
    /// `/dev/null` or `/dev/zero`: the same bytes on every machine.
    Constant,
    /// `/dev/random` or `/dev/urandom`.
    Random,
    /// Anywhere else: the absolute path.
    Outside(&'p str),
}

/// How a file is opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    /// Write, read and write, create, truncate or append.
    Write,
}

/// Which clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Clock {
    /// The wall clock (`CLOCK_REALTIME`, `SystemTime`).
    Wall,
    /// `CLOCK_MONOTONIC` and its kin (`Instant`).
    Monotonic,
    /// The process's or the thread's CPU time.
    Cpu,
}

/// The other end of a socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Peer<'p> {
    /// A loopback address, at a port this test bound itself (the wrapper tracks its binds).
    OwnLoopback,
    /// A loopback address at a port this test did not bind: a service of the host's.
    HostLoopback,
    /// Any address off the machine: `ip:port` as text.
    Remote(&'p str),
    /// A Unix socket at this place.
    Unix(Where<'p>),
}

/// One observed operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op<'p> {
    Open { at: Where<'p>, access: Access },
    /// `stat`, `lstat`, `access`, `readlink`: the file's facts, not its bytes.
    Stat { at: Where<'p> },
    ReadDir { at: Where<'p> },
    /// A file removed or renamed, a folder made: a write to the place.
    Change { at: Where<'p> },
    Clock(Clock),
    /// `getenv`: the variable's name.
    Env { name: &'p str },
    /// `getrandom`, `arc4random`, `SecRandomCopyBytes`.
    Random,
    Connect { peer: Peer<'p> },
    /// `bind` or `listen` at a loopback address (`true`) or another (`false`).
    Bind { loopback: bool },
    /// A program started: the path.
    Spawn { program: &'p str },
    /// A read through a descriptor the wrapper did not see opened (inherited, or made by a
    /// call it does not wrap).
    UnseenDescriptor,
    /// A call into a foreign function no wrapper binds, by its symbol.
    Foreign { symbol: &'p str },
}

/// What an ask names, before it is hashed into a `ReadName` (with its kind, so a file and a
/// variable of one name differ).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked<'p> {
    /// A file's bytes, by its path from the workspace root.
    File(&'p str),
    /// A file's facts (kind, length, mode; never its times), by its path from the root.
    Facts(&'p str),
    /// A folder's sorted names, by its path from the root.
    Listing(&'p str),
    /// A file outside the root by its absolute path: an ask, answered like any other, but one
    /// that holds the machine's layout, so CI's result for it meets a developer's only when the
    /// two paths are equal.
    OutsideFile(&'p str),
    OutsideFacts(&'p str),
    OutsideListing(&'p str),
    /// An environment variable, set or not.
    Var(&'p str),
}

/// The class of one operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class<'p> {
    Ask(Asked<'p>),
    /// Nothing outside the test is read: no ask, no reason.
    Own,
    NotKept(NotKept),
    /// Refused: the wrapper fails the call (`EPERM`) and the test ends as refused, naming
    /// `what` (the sandbox would refuse it anyway; the wrapper is how it is named).
    Refused { refusal: Refusal, what: &'p str },
}

/// What a wrapper does after [`Observe::saw`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Allowed {
    /// Make the real call.
    Go,
    /// Do not make it: the child is ending as refused.
    Stop,
}

/// What the launcher's recording wrappers call inside a forked child, before each real call.
/// `child::Child` implements it: it classifies the call, keeps the asks and the first not-kept
/// reason, and on a refusal ends the test.
pub trait Observe {
    fn saw(&mut self, op: Op<'_>) -> Allowed;
}

/// An operation's kind: the first column of [`RULES`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    OpenRead,
    OpenWrite,
    Stat,
    ReadDir,
    Change,
    ClockWall,
    ClockMonotonic,
    ClockCpu,
    Env,
    Random,
    ConnectOwnLoopback,
    ConnectHostLoopback,
    ConnectRemote,
    ConnectUnix,
    BindLoopback,
    BindOther,
    Spawn,
    UnseenDescriptor,
    Foreign,
}

/// Where an operation's path is: the second column ([`Site::None`] for an operation with no
/// path).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Site {
    None,
    Root,
    Temp,
    Constant,
    Random,
    Outside,
}

/// A rule's verdict, before the operation's text is put in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rule {
    AskFile,
    AskFacts,
    AskListing,
    AskOutsideFile,
    AskOutsideFacts,
    AskOutsideListing,
    AskVar,
    Own,
    NotKept(NotKept),
    Refuse(Refusal),
}

/// Every (kind, site) pair an [`Op`] can give, once. `table_covers_every_op` holds it to that.
pub const RULES: &[(Kind, Site, Rule)] = &[
    (Kind::OpenRead, Site::Root, Rule::AskFile),
    (Kind::OpenRead, Site::Temp, Rule::Own),
    (Kind::OpenRead, Site::Constant, Rule::Own),
    (Kind::OpenRead, Site::Random, Rule::Own),
    (Kind::OpenRead, Site::Outside, Rule::AskOutsideFile),
    (Kind::OpenWrite, Site::Root, Rule::Refuse(Refusal::Write)),
    (Kind::OpenWrite, Site::Temp, Rule::Own),
    (Kind::OpenWrite, Site::Constant, Rule::Own),
    (Kind::OpenWrite, Site::Random, Rule::Own),
    (Kind::OpenWrite, Site::Outside, Rule::Refuse(Refusal::Write)),
    (Kind::Stat, Site::Root, Rule::AskFacts),
    (Kind::Stat, Site::Temp, Rule::Own),
    (Kind::Stat, Site::Constant, Rule::Own),
    (Kind::Stat, Site::Random, Rule::Own),
    (Kind::Stat, Site::Outside, Rule::AskOutsideFacts),
    (Kind::ReadDir, Site::Root, Rule::AskListing),
    (Kind::ReadDir, Site::Temp, Rule::Own),
    (Kind::ReadDir, Site::Constant, Rule::Own),
    (Kind::ReadDir, Site::Random, Rule::Own),
    (Kind::ReadDir, Site::Outside, Rule::AskOutsideListing),
    (Kind::Change, Site::Root, Rule::Refuse(Refusal::Write)),
    (Kind::Change, Site::Temp, Rule::Own),
    (Kind::Change, Site::Constant, Rule::Refuse(Refusal::Write)),
    (Kind::Change, Site::Random, Rule::Refuse(Refusal::Write)),
    (Kind::Change, Site::Outside, Rule::Refuse(Refusal::Write)),
    (Kind::ClockWall, Site::None, Rule::NotKept(NotKept::Clock)),
    (Kind::ClockMonotonic, Site::None, Rule::NotKept(NotKept::Clock)),
    (Kind::ClockCpu, Site::None, Rule::NotKept(NotKept::Clock)),
    (Kind::Env, Site::None, Rule::AskVar),
    (Kind::Random, Site::None, Rule::Own),
    (Kind::ConnectOwnLoopback, Site::None, Rule::Own),
    (Kind::ConnectHostLoopback, Site::None, Rule::NotKept(NotKept::Network)),
    (Kind::ConnectRemote, Site::None, Rule::Refuse(Refusal::Connect)),
    (Kind::ConnectUnix, Site::Root, Rule::NotKept(NotKept::Network)),
    (Kind::ConnectUnix, Site::Temp, Rule::Own),
    (Kind::ConnectUnix, Site::Constant, Rule::NotKept(NotKept::Network)),
    (Kind::ConnectUnix, Site::Random, Rule::NotKept(NotKept::Network)),
    (Kind::ConnectUnix, Site::Outside, Rule::NotKept(NotKept::Network)),
    (Kind::BindLoopback, Site::None, Rule::Own),
    (Kind::BindOther, Site::None, Rule::Refuse(Refusal::Connect)),
    (Kind::Spawn, Site::None, Rule::NotKept(NotKept::Foreign)),
    (Kind::UnseenDescriptor, Site::None, Rule::NotKept(NotKept::Descriptor)),
    (Kind::Foreign, Site::None, Rule::NotKept(NotKept::Foreign)),
];

/// The (kind, site) of an operation, and the text a verdict names (a path, a name, a peer).
pub fn key<'p>(op: &Op<'p>) -> (Kind, Site, &'p str) {
    let place = |at: Where<'p>| match at {
        Where::Root(path) => (Site::Root, path),
        Where::Temp => (Site::Temp, ""),
        Where::Constant => (Site::Constant, ""),
        Where::Random => (Site::Random, ""),
        Where::Outside(path) => (Site::Outside, path),
    };
    match *op {
        Op::Open { at, access } => {
            let (site, text) = place(at);
            let kind = match access {
                Access::Read => Kind::OpenRead,
                Access::Write => Kind::OpenWrite,
            };
            (kind, site, text)
        }
        Op::Stat { at } => {
            let (site, text) = place(at);
            (Kind::Stat, site, text)
        }
        Op::ReadDir { at } => {
            let (site, text) = place(at);
            (Kind::ReadDir, site, text)
        }
        Op::Change { at } => {
            let (site, text) = place(at);
            (Kind::Change, site, text)
        }
        Op::Clock(Clock::Wall) => (Kind::ClockWall, Site::None, ""),
        Op::Clock(Clock::Monotonic) => (Kind::ClockMonotonic, Site::None, ""),
        Op::Clock(Clock::Cpu) => (Kind::ClockCpu, Site::None, ""),
        Op::Env { name } => (Kind::Env, Site::None, name),
        Op::Random => (Kind::Random, Site::None, ""),
        Op::Connect { peer: Peer::OwnLoopback } => (Kind::ConnectOwnLoopback, Site::None, ""),
        Op::Connect { peer: Peer::HostLoopback } => (Kind::ConnectHostLoopback, Site::None, ""),
        Op::Connect { peer: Peer::Remote(address) } => (Kind::ConnectRemote, Site::None, address),
        Op::Connect { peer: Peer::Unix(at) } => {
            let (site, text) = place(at);
            (Kind::ConnectUnix, site, text)
        }
        Op::Bind { loopback: true } => (Kind::BindLoopback, Site::None, ""),
        Op::Bind { loopback: false } => (Kind::BindOther, Site::None, ""),
        Op::Spawn { program } => (Kind::Spawn, Site::None, program),
        Op::UnseenDescriptor => (Kind::UnseenDescriptor, Site::None, ""),
        Op::Foreign { symbol } => (Kind::Foreign, Site::None, symbol),
    }
}

/// The rule of a (kind, site) pair; `None` for a pair no [`Op`] gives.
pub fn rule(kind: Kind, site: Site) -> Option<Rule> {
    RULES.iter().find(|(k, s, _)| *k == kind && *s == site).map(|(_, _, rule)| *rule)
}

/// The class of `op`. Every [`Op`] has a row ([`RULES`]); a pair without one is refused by name
/// as a call the profile does not allow, never guessed.
pub fn classify<'p>(op: &Op<'p>) -> Class<'p> {
    let (kind, site, text) = key(op);
    let Some(rule) = rule(kind, site) else { return Class::Refused { refusal: Refusal::Call, what: kind_name(kind) } };
    match rule {
        Rule::AskFile => Class::Ask(Asked::File(text)),
        Rule::AskFacts => Class::Ask(Asked::Facts(text)),
        Rule::AskListing => Class::Ask(Asked::Listing(text)),
        Rule::AskOutsideFile => Class::Ask(Asked::OutsideFile(text)),
        Rule::AskOutsideFacts => Class::Ask(Asked::OutsideFacts(text)),
        Rule::AskOutsideListing => Class::Ask(Asked::OutsideListing(text)),
        Rule::AskVar => Class::Ask(Asked::Var(text)),
        Rule::Own => Class::Own,
        Rule::NotKept(why) => Class::NotKept(why),
        Rule::Refuse(refusal) => Class::Refused { refusal, what: refused_what(op, text) },
    }
}

/// What a refusal names: the path, the peer; a write to a constant device names the device.
fn refused_what<'p>(op: &Op<'p>, text: &'p str) -> &'p str {
    match *op {
        Op::Change { at: Where::Constant } => "/dev/null",
        Op::Change { at: Where::Random } => "/dev/urandom",
        Op::Bind { loopback: false } => "a non-loopback address",
        _ => text,
    }
}

/// A kind's name, for a refusal of a pair without a rule.
pub const fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::OpenRead => "open for reading",
        Kind::OpenWrite => "open for writing",
        Kind::Stat => "stat",
        Kind::ReadDir => "readdir",
        Kind::Change => "a change to a folder",
        Kind::ClockWall => "the wall clock",
        Kind::ClockMonotonic => "the monotonic clock",
        Kind::ClockCpu => "a CPU clock",
        Kind::Env => "getenv",
        Kind::Random => "getrandom",
        Kind::ConnectOwnLoopback | Kind::ConnectHostLoopback | Kind::ConnectRemote | Kind::ConnectUnix => "connect",
        Kind::BindLoopback | Kind::BindOther => "bind",
        Kind::Spawn => "spawn",
        Kind::UnseenDescriptor => "a read through an unseen descriptor",
        Kind::Foreign => "a foreign call",
    }
}

/// The more telling of two not-kept reasons, so a run's reason does not depend on the order
/// its calls came in: the lesser by [`NotKept`]'s declaration order.
pub const fn first_reason(a: NotKept, b: NotKept) -> NotKept {
    match (a as u8) <= (b as u8) {
        true => a,
        false => b,
    }
}
