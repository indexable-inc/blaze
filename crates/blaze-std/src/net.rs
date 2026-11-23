//! Connections (docs/v2/plan/design/std.md, `Net`): connects and listens on blaze-core's
//! sockets, narrowed by [`Net::connect_only`] and [`Net::listen_only`], which never widen.
//!
//! Addresses are a [`HostPort`] for TCP and a [`Path`] beneath the driver's root for a Unix
//! socket, each checked as it is made, never a string or a socket address parsed from one.
//! blaze-core has no effect for name lookup yet, so a host that is a DNS name, not an IP
//! address, is refused ([`NetKind::NoLookup`]) until it has.
//!
//! **Finishing.** A [`Conn`] and a [`Listener`] are linear (`#[blaze::linear]`): leaving one is
//! a compile error, and each ends with a call that waits for its end and says how it went:
//! [`Conn::finish`] hangs up, [`Listener::close`] stops listening. A panic that unwinds past
//! one runs its abandonment path, its `Drop`: the connection is hung up, or the listen ended,
//! without waiting, and what comes of either is dropped as it arrives. There is no abortive
//! close (`reset`, TCP's RST) yet: blaze-core has no effect for one. Receives and sends in
//! flight are [`Pending`] values ([`Conn::read_into`], [`Conn::write_from`]).
//!
//! There is no UDP socket ([`Net`]'s `Datagrams`) yet: blaze-core's `Effect` has no datagram
//! effects (a bind of a UDP socket, a send to an address, a receive with its sender).

mod datagrams;
pub mod options;

use alloc::{alloc::Global, boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::fmt;

use blaze_core::{
    Address, Done, Effect, Tag,
    trust::{Roots, TrustError},
};
pub use blaze_core::dgram::{Batch, DgramError, Ecn, Head, Options as DgramOptions, Slots};
pub use blaze_core::sockopt::{Ask, Linger, MulticastV4, Tcp, TcpOptions, UdpOptions};
pub use datagrams::{Datagrams, DatagramsOrAccept, RecvArm};

use crate::{
    Plant, Stopped,
    budget::{Budget, Over},
    cx::Cx,
    env::{Env, NotAllowed, VarName},
    io::{Ends, Io},
    mem::Bytes,
    path::Path,
    pending::{self, Pending},
    resolver::Resolver,
    unfinished::Unfinished,
};

/// Where to connect or listen.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount or a copy
pub enum Addr {
    /// A TCP host and port. Its host must be an IP address until blaze-core can look a name up.
    Tcp(HostPort),
    /// A Unix socket beneath the driver's root.
    Unix(Path),
    /// A UDP host and port.
    Udp(HostPort),
}

impl Addr {
    /// The driver's address, or `None` for a host that would need a name lookup.
    fn address(&self) -> Option<Address> {
        match self {
            Addr::Tcp(at) => at.socket_addr().map(Address::Tcp),
            // clone: the socket's name, a refcount.
            Addr::Unix(path) => Some(Address::Unix(path.name().clone())),
            Addr::Udp(_) => None,
        }
    }

    /// The driver's address, or the error of `op` that it needs a name lookup or is not a stream.
    fn reach(&self, op: NetOp) -> Result<Address, NetError> {
        if matches!(self, Addr::Udp(_)) {
            return Err(error(op, self, NetKind::NotStream));
        }
        self.address().ok_or_else(|| error(op, self, NetKind::NoLookup))
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Addr::Tcp(at) => write!(f, "tcp:{at}"),
            Addr::Unix(path) => write!(f, "unix:{path}"),
            Addr::Udp(at) => write!(f, "udp:{at}"),
        }
    }
}

/// Hosts and ports, checked as they are made: blaze-url's, their one home (rule
/// one-concept-one-home), here under bstd's names. [`crate::host!`] checks a literal as the
/// crate compiles with [`check_literal`].
pub use blaze_url::host::{HOST_MOST, HostPort, HostPortError, HostPortWhy, check_literal, refused, refused_host};

/// A [`HostPort`] from a string literal, checked as the crate compiles:
/// `host!("api.stripe.com:443")`. A bad literal (`host!("a..b:443")`) is a compile error
/// saying why.
#[macro_export]
macro_rules! host {
    ($raw:literal) => {{
        const _: () = $crate::net::check_literal($raw);
        $crate::net::HostPort::parse($raw).expect("a host! literal was checked as the crate compiled")
    }};
}

/// Which addresses a [`Net`] may reach one way.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount or a copy
pub(crate) enum Allowed {
    Any,
    Only(Addr),
    Nothing,
}

impl Allowed {
    /// Only `at`, of what this allows: never more.
    fn narrowed(&self, at: &Addr) -> Allowed {
        match self {
            Allowed::Any => Allowed::Only(at.clone()),
            Allowed::Only(only) if only == at => Allowed::Only(at.clone()),
            Allowed::Only(Addr::Tcp(..) | Addr::Unix(..) | Addr::Udp(..)) | Allowed::Nothing => Allowed::Nothing,
        }
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn allows(&self, at: &Addr) -> bool {
        match self {
            Allowed::Any => true,
            Allowed::Only(only) => only == at,
            Allowed::Nothing => false,
        }
    }
}

/// What a network call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetOp {
    Connect,
    Listen,
    Accept,
    Bind,
    Send,
    Recv,
    /// [`Listener::close`] or [`Datagrams::close`].
    Close,
    /// [`Conn::finish`].
    Finish,
    /// [`crate::resolver::Resolver::lookup`].
    Lookup,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum NetKind {
    /// The capability does not reach the address (rule: narrowing never widens).
    NotAllowed,
    /// A batch contained a datagram to a peer this `Net` does not reach.
    PeerNotAllowed { peer: core::net::SocketAddr },
    /// A UDP address was passed to a stream (`connect`/`listen`) operation.
    NotStream,
    /// A socket option was refused before the socket was made.
    Unavailable(blaze_core::sockopt::Unavailable),
    /// A datagram socket effect failed.
    Datagrams(DgramError),
    /// The host is a DNS name, and blaze-core has no effect that looks one up yet.
    NoLookup,
    /// The name service has no such name.
    NoSuchName,
    /// The `Net` it came from has a byte limit ([`Net::limit`]) with no room for it.
    OverLimit(Over),
    /// [`Listener::close`] took more than `most` connections made while it closed (the
    /// world's [`crate::io::Limits::close_most`]), and stopped waiting for the listen's end.
    Unending { accepted: usize, most: usize },
    Stopped(Stopped),
    /// The driver failed it, saying this.
    Driver(String),
    /// The driver answered with a completion of another kind: a driver's bug.
    Answered(String),
}

/// A network call that failed: the call, the address and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct NetError {
    pub op: NetOp,
    pub addr: Addr,
    pub kind: NetKind,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (op, addr) = (self.op, &self.addr);
        match &self.kind {
            NetKind::NotAllowed => {
                write!(f, "bstd::net {op:?} to {addr}: this Net does not reach it (it was narrowed)")
            }
            NetKind::PeerNotAllowed { peer } => {
                write!(f, "bstd::net {op:?} from {addr}: this Net does not reach peer {peer} (it was narrowed)")
            }
            NetKind::NotStream => {
                write!(f, "bstd::net {op:?} to {addr}: a UDP address cannot be used for a stream socket")
            }
            NetKind::Unavailable(refused) => {
                write!(f, "bstd::net {op:?} at {addr}: {refused}")
            }
            NetKind::Datagrams(failed) => {
                write!(f, "bstd::net {op:?} at {addr}: {failed}")
            }
            NetKind::NoLookup => write!(
                f,
                "bstd::net {op:?} to {addr}: its host is a name, and blaze-core has no effect that \
                 looks a name up yet: give its IP address"
            ),
            NetKind::NoSuchName => write!(f, "bstd::net {op:?} of {addr}: the name service has no such name"),
            NetKind::OverLimit(over) => {
                write!(f, "bstd::net {op:?} to {addr}: this Net's byte limit: {over}")
            }
            NetKind::Unending { accepted, most } => write!(
                f,
                "bstd::net {op:?} to {addr}: {accepted} connections were made while it closed, more \
                 than {most} (the world's Limits::close_most), and the listen's end never came \
                 (rule bounded-progress)"
            ),
            NetKind::Stopped(stopped) => write!(f, "bstd::net {op:?} to {addr}: {stopped}"),
            NetKind::Driver(said) => write!(f, "bstd::net {op:?} to {addr}: {said}"),
            NetKind::Answered(done) => {
                write!(f, "bstd::net {op:?} to {addr}: the driver answered {done} (a driver's bug)")
            }
        }
    }
}

impl core::error::Error for NetError {}

#[blaze::nonblocking]
pub(crate) fn error(op: NetOp, addr: &Addr, kind: NetKind) -> NetError {
    // clone: an error's address.
    NetError { op, addr: addr.clone(), kind }
}

fn unexpected(op: NetOp, addr: &Addr, done: Done) -> NetError {
    let kind = match done {
        Done::Failed(failed) => NetKind::Driver(failed.0),
        Done::Broken { error: broken, .. } => NetKind::Driver(broken.0),
        // alloc: an error's message.
        done => NetKind::Answered(format!("{done:?}")),
    };
    error(op, addr, kind)
}

/// The network, from [`Io::net`].
#[derive(Clone)]
pub struct Net {
    io: Io,
    connect: Allowed,
    listen: Allowed,
    bind: Allowed,
    /// The bytes its connections may still send and receive, if it has a limit.
    budget: Option<Arc<Budget>>,
}

impl Net {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(io: Io) -> Net {
        Net { io, connect: Allowed::Any, listen: Allowed::Any, bind: Allowed::Any, budget: None }
    }

    /// The world it is in.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn io(&self) -> &Io {
        &self.io
    }

    /// Name lookup alone, for the hosts this may connect to: for code that should resolve and
    /// not connect (docs/v2/plan/design/std.md, "The rest of the set", `Resolver`).
    pub fn resolver(&self) -> Resolver {
        // clone: authority held, narrowed: a refcount or a copy.
        Resolver::new(self.io.clone(), self.connect.clone())
    }

    /// Connects only to `at`, of what this connects to, and listens nowhere.
    pub fn connect_only(&self, at: &Addr) -> Net {
        // clone: the budget shared, a refcount.
        Net {
            io: self.io.clone(),
            connect: self.connect.narrowed(at),
            listen: Allowed::Nothing,
            bind: Allowed::Nothing,
            budget: self.budget.clone(),
        }
    }

    /// Listens only on `at`, of what this listens on, and connects nowhere.
    pub fn listen_only(&self, at: &Addr) -> Net {
        // clone: the budget shared, a refcount.
        Net {
            io: self.io.clone(),
            connect: Allowed::Nothing,
            listen: self.listen.narrowed(at),
            bind: Allowed::Nothing,
            budget: self.budget.clone(),
        }
    }

    /// The same reach with a byte limit: every connection made from the `Net` this returns,
    /// and from any narrowed from it, sends and receives at most `bytes` between them, counted
    /// by bstd. A send or receive with no room left ends in [`NetKind::OverLimit`], and
    /// nothing of it is sent. A limit set on a `Net` that has one is inside it: both count,
    /// so it never raises what is left (docs/v2/plan/design/std.md, "Budgets ride the same
    /// way"). A rate limit is not written yet.
    pub fn limit(&self, bytes: u64) -> Net {
        // clone: authority held, narrowed: refcounts or copies.
        let (connect, listen, bind) = (self.connect.clone(), self.listen.clone(), self.bind.clone());
        // clone: the budget this one is inside, a refcount.
        Net { io: self.io.clone(), connect, listen, bind, budget: Some(Budget::new(bytes, self.budget.clone())) }
    }

    /// The bytes its connections may still send and receive; `None` with no limit.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn limit_left(&self) -> Option<u64> {
        self.budget.as_deref().map(Budget::left)
    }

    /// A connection to `at` with default TCP options: finished with [`Conn::finish`].
    #[track_caller]
    pub fn connect(&self, cx: &Cx, at: &Addr) -> Result<Conn, NetError> {
        self.connect_with(cx, at, blaze_core::sockopt::Tcp::DEFAULT)
    }

    /// A connection to `at` with explicit socket `options` (e.g. `nodelay`, `linger: Linger::Reset`,
    /// keepalive, buffers): finished with [`Conn::finish`].
    #[track_caller]
    pub fn connect_with(&self, cx: &Cx, at: &Addr, options: blaze_core::sockopt::Tcp) -> Result<Conn, NetError> {
        if !self.connect.allows(at) {
            return Err(error(NetOp::Connect, at, NetKind::NotAllowed));
        }
        let address = at.reach(NetOp::Connect)?;
        let done = self
            .io
            .call(cx, |tag| Effect::Connect { at: address, options, tag })
            .map_err(|stopped| error(NetOp::Connect, at, NetKind::Stopped(stopped)))?;
        match done {
            // clone: the connection's peer, for its errors.
            Done::Connected { conn } => Ok(Conn::new(&self.io, conn, at.clone(), self.budget.clone())),
            done => Err(unexpected(NetOp::Connect, at, done)),
        }
    }

    /// Listens on `at` with default TCP options: each connection made to it is
    /// [`Listener::accept`]ed. Finished with [`Listener::close`].
    #[track_caller]
    pub fn listen(&self, cx: &Cx, at: &Addr) -> Result<Listener, NetError> {
        self.listen_with(cx, at, blaze_core::sockopt::Tcp::DEFAULT)
    }

    /// Listens on `at` with explicit socket `options`: finished with [`Listener::close`].
    #[track_caller]
    pub fn listen_with(&self, cx: &Cx, at: &Addr, options: blaze_core::sockopt::Tcp) -> Result<Listener, NetError> {
        if !self.listen.allows(at) {
            return Err(error(NetOp::Listen, at, NetKind::NotAllowed));
        }
        cx.check().map_err(|stopped| error(NetOp::Listen, at, NetKind::Stopped(stopped)))?;
        let address = at.reach(NetOp::Listen)?;
        let tag = self.io.submit(|tag| Effect::Listen { at: address, options, tag });
        let listening = Listening { io: self.io.clone(), tag, open: true };
        // clone: the listener's address, for its errors; the budget shared, a refcount.
        Ok(Listener {
            listening,
            at: at.clone(),
            budget: self.budget.clone(),
            unfinished: Unfinished::new("net::Listener", "close(cx)"),
        })
    }

    /// The machine's TLS trust anchors, read now as one [`Effect::Roots`]: macOS's keychains,
    /// or on Linux `SSL_CERT_FILE` and `SSL_CERT_DIR` as `env` gives them, then the known
    /// bundles. Ask on a client's first connection, never at start, and keep the answer for
    /// the client's life: each call reads the store again. `env` is the asker's, passed in
    /// (rule 75): an `env` narrowed away from either variable is refused, not read as unset.
    pub fn roots(&self, cx: &Cx, env: &Env) -> Result<Roots<Global>, RootsError> {
        let var = |name: &'static str| {
            let name = VarName::new(name).expect("SSL_CERT_FILE and SSL_CERT_DIR are names");
            // alloc: the effect's copy of a path, once per read of the store.
            env.get(&name).map(|value| value.map(String::from)).map_err(RootsError::NotAllowed)
        };
        let (cert_file, cert_dir) = (var("SSL_CERT_FILE")?, var("SSL_CERT_DIR")?);
        match self.io.call(cx, |tag| Effect::Roots { cert_file, cert_dir, tag }).map_err(RootsError::Stopped)? {
            Done::Roots(roots) => Ok(*roots),
            Done::Untrusted(error) => Err(RootsError::Untrusted(error)),
            Done::Failed(failed) => Err(RootsError::Driver(failed)),
            // alloc: an error's message.
            done => Err(RootsError::Answered(format!("{done:?}"))),
        }
    }
}

/// What [`Net::roots`] found instead of trust anchors.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum RootsError {
    /// The `env` it was given may not read `SSL_CERT_FILE` or `SSL_CERT_DIR`.
    NotAllowed(NotAllowed),
    /// No root, or a store that would not read: every place looked, and what each gave.
    Untrusted(Box<TrustError>),
    Stopped(Stopped),
    /// The driver refused it (a host whose store blaze-core does not read), saying this.
    Driver(blaze_core::IoError),
    /// The driver answered with a completion of another kind: a driver's bug.
    Answered(String),
}

impl fmt::Display for RootsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootsError::NotAllowed(refused) => write!(f, "bstd::net roots: {refused}"),
            RootsError::Untrusted(error) => write!(f, "bstd::net roots: {error}"),
            RootsError::Stopped(stopped) => write!(f, "bstd::net roots: {stopped}"),
            RootsError::Driver(said) => write!(f, "bstd::net roots: {}", said.0),
            RootsError::Answered(done) => write!(f, "bstd::net roots: the driver answered {done} (a driver's bug)"),
        }
    }
}

impl core::error::Error for RootsError {}

/// A listen in flight, finished with [`Listener::close`]. Linear (see the module's doc): a
/// panic that drops one ends the listen without waiting, and drops what comes of it.
#[blaze::linear]
#[must_use = "a listener is finished with close(cx)"]
pub struct Listener {
    listening: Listening,
    at: Addr,
    /// The byte limit of the `Net` it came from, which its connections count against.
    budget: Option<Arc<Budget>>,
    unfinished: Unfinished,
}

/// A listen's tag and its abandonment path.
struct Listening {
    io: Io,
    tag: Tag,
    /// Whether the listen may still bring completions: cleared when it fails or is closed.
    open: bool,
}

impl Drop for Listening {
    fn drop(&mut self) {
        if self.open {
            let tag = self.tag;
            self.io.forget(tag, Ends::Stream);
            let _ = self.io.submit(|_| Effect::Unlisten { tag });
        }
    }
}

impl Listener {
    /// The next connection made to it: finished with [`Conn::finish`].
    #[track_caller]
    pub fn accept(&mut self, cx: &Cx) -> Result<Conn, NetError> {
        let done = cx
            .wait(&self.listening.io, self.listening.tag, Ends::Stream)
            .map_err(|stopped| error(NetOp::Accept, &self.at, NetKind::Stopped(stopped)))?;
        self.accept_done(done)
    }

    pub(super) fn accept_tag(&self) -> Tag {
        self.listening.tag
    }

    pub(super) fn accept_done(&mut self, done: Done) -> Result<Conn, NetError> {
        let listening = &mut self.listening;
        match done {
            // clone: the connection's address, for its errors.
            Done::Accepted { conn } => Ok(Conn::new(&listening.io, conn, self.at.clone(), self.budget.clone())),
            done => {
                // A failed listen has ended: nothing more comes of its tag.
                listening.open = false;
                Err(unexpected(NetOp::Accept, &self.at, done))
            }
        }
    }

    /// The listen ended: it waits, until `cx`'s deadline, for the driver to say so, hanging up
    /// each connection made meanwhile, at most the world's
    /// [`crate::io::Limits::close_most`] of them. The one way a
    /// `Listener` ends.
    #[blaze::consumes]
    pub fn close(mut self, cx: &Cx) -> Result<(), NetError> {
        self.unfinished.finished();
        let listening = &mut self.listening;
        if !listening.open {
            return Ok(());
        }
        listening.open = false;
        let (io, tag) = (listening.io.clone(), listening.tag);
        let _ = io.submit(|_| Effect::Unlisten { tag });
        let most = io.limits().close_most;
        let mut accepted = 0;
        while accepted <= most {
            let done = match cx.wait(&io, tag, Ends::Stream) {
                Ok(done) => done,
                Err(stopped) => {
                    io.forget(tag, Ends::Stream);
                    return Err(error(NetOp::Close, &self.at, NetKind::Stopped(stopped)));
                }
            };
            match done {
                Done::Unlistened => return Ok(()),
                Done::Accepted { conn } => {
                    accepted += 1;
                    let hangup = io.submit(|tag| Effect::Hangup { conn, tag });
                    io.forget(hangup, Ends::Once);
                }
                done => return Err(unexpected(NetOp::Close, &self.at, done)),
            }
        }
        io.forget(tag, Ends::Stream);
        Err(error(NetOp::Close, &self.at, NetKind::Unending { accepted, most }))
    }
}

/// A connection, finished with [`Conn::finish`]. Linear (see the module's doc): a panic that
/// drops one hangs it up without waiting.
#[blaze::linear]
#[must_use = "a connection is finished with finish(cx)"]
pub struct Conn {
    link: Link,
    peer: Addr,
    /// The byte limit of the `Net` it came from, which its sends and receives count against.
    budget: Option<Arc<Budget>>,
    unfinished: Unfinished,
}

/// A connection's driver handle and its abandonment path.
struct Link {
    io: Io,
    conn: blaze_core::Conn,
    /// Whether it is still to be hung up: cleared by [`Conn::finish`].
    open: bool,
}

impl Drop for Link {
    fn drop(&mut self) {
        if self.open {
            let conn = self.conn;
            let tag = self.io.submit(|tag| Effect::Hangup { conn, tag });
            self.io.forget(tag, Ends::Once);
        }
    }
}

/// Completion state for [`Conn::write_from`], stored inline inside [`Pending`] (zero heap alloc).
pub enum WriteFrom<'buf> {
    InFlight { peer: &'buf Addr, budget: Option<&'buf Budget>, asked: u64, after: bool },
    Refused(NetError),
}

impl<'buf> pending::Complete<'buf> for WriteFrom<'buf> {
    type Output = Result<(), NetError>;

    fn complete(self, _: &'buf mut [u8], done: Result<Done, Stopped>) -> Self::Output {
        match self {
            WriteFrom::Refused(err) => Err(err),
            WriteFrom::InFlight { peer, budget, asked, after } => match done {
                Ok(Done::Sent(_)) => match budget {
                    Some(budget) if after => budget.take(asked).map_err(|over| error(NetOp::Send, peer, NetKind::OverLimit(over))),
                    _ => Ok(()),
                },
                Ok(done) => Err(unexpected(NetOp::Send, peer, done)),
                Err(stopped) => Err(error(NetOp::Send, peer, NetKind::Stopped(stopped))),
            },
        }
    }
}

/// Completion state for [`Conn::read_into`], stored inline inside [`Pending`] (zero heap alloc).
pub enum ReadInto<'buf> {
    InFlight { peer: &'buf Addr, budget: Option<&'buf Budget>, most: usize },
    Refused(NetError),
}

impl<'buf> pending::Complete<'buf> for ReadInto<'buf> {
    type Output = Result<usize, NetError>;

    fn complete(self, into: &'buf mut [u8], done: Result<Done, Stopped>) -> Self::Output {
        match self {
            ReadInto::Refused(err) => Err(err),
            ReadInto::InFlight { peer, budget, most } => match done {
                Ok(Done::Read(bytes)) if bytes.len() <= most => {
                    into[..bytes.len()].copy_from_slice(&bytes);
                    if let Some(budget) = budget {
                        budget.give_back((most - bytes.len()) as u64);
                    }
                    Ok(bytes.len())
                }
                Ok(done) => {
                    if let Some(budget) = budget {
                        budget.give_back(most as u64);
                    }
                    Err(unexpected(NetOp::Recv, peer, done))
                }
                Err(stopped) => {
                    if let Some(budget) = budget {
                        budget.give_back(most as u64);
                    }
                    Err(error(NetOp::Recv, peer, NetKind::Stopped(stopped)))
                }
            },
        }
    }
}

impl Conn {
    #[track_caller]
    fn new(io: &Io, conn: blaze_core::Conn, peer: Addr, budget: Option<Arc<Budget>>) -> Conn {
        Conn {
            link: Link { io: io.clone(), conn, open: true },
            peer,
            budget,
            unfinished: Unfinished::new("net::Conn", "finish(cx)"),
        }
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn peer(&self) -> &Addr {
        &self.peer
    }

    /// A send of all of `from`, in flight. The bytes are copied as it starts (see
    /// [`crate::pending`]), and stay lent until it ends. Under a byte limit ([`Net::limit`])
    /// the bytes are taken from it before anything is sent; with no room for them the send
    /// never starts, and its outcome is [`NetKind::OverLimit`].
    #[track_caller]
    pub fn write_from<'buf>(&'buf self, from: &'buf [u8]) -> Pending<'buf, WriteFrom<'buf>> {
        let conn = self.link.conn;
        let peer = &self.peer;
        let asked = from.len() as u64;
        // The planted bug of the budget tests: the limit is looked at once the bytes are gone.
        let after = self.link.io.plant() == Plant::BudgetCheckedAfter;
        let budget = self.budget.as_deref();
        let short = match budget {
            Some(budget) if !after => budget.take(asked).err(),
            _ => None,
        };
        if let Some(short) = short {
            return pending::refused(&self.link.io, "bstd::net write_from", WriteFrom::Refused(error(NetOp::Send, peer, NetKind::OverLimit(short))));
        }
        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
        let bytes = from.to_vec();
        let effect = |tag| Effect::Send { conn, bytes, tag };
        pending::start(&self.link.io, "bstd::net write_from", &mut [], effect, WriteFrom::InFlight { peer, budget, asked, after })
    }

    /// All of `bytes` sent, or an error: never a part said sent. Its [`Conn::write_from`],
    /// waited for. The bytes are handed to the driver as one send with nothing kept back or
    /// joined to another: there is no buffer between a call and the socket.
    pub fn send_all(&self, cx: &Cx, bytes: &[u8]) -> Result<(), NetError> {
        cx.stopped_by(self.link.io.now())
            .map_err(|stopped| error(NetOp::Send, &self.peer, NetKind::Stopped(stopped)))?;
        self.write_from(bytes).wait(cx)
    }

    /// Sends all `parts` in order as one send, joining once directly into the effect's buffer
    /// (avoiding a separate caller allocation for header + payload framing).
    pub fn send_vectored(&self, cx: &Cx, parts: &[&[u8]]) -> Result<(), NetError> {
        cx.stopped_by(self.link.io.now())
            .map_err(|stopped| error(NetOp::Send, &self.peer, NetKind::Stopped(stopped)))?;
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let asked = total as u64;
        let after = self.link.io.plant() == Plant::BudgetCheckedAfter;
        let budget = self.budget.as_deref();
        if let Some(budget) = budget
            && !after
            && let Err(short) = budget.take(asked)
        {
            return Err(error(NetOp::Send, &self.peer, NetKind::OverLimit(short)));
        }
        // alloc: the effect's bytes, filled once directly from `parts`.
        let mut bytes = Vec::with_capacity(total);
        for part in parts {
            bytes.extend_from_slice(part);
        }
        let conn = self.link.conn;
        pending::start(
            &self.link.io,
            "bstd::net send_vectored",
            &mut [],
            |tag| Effect::Send { conn, bytes, tag },
            WriteFrom::InFlight { peer: &self.peer, budget, asked, after },
        )
        .wait(cx)
    }

    /// A receive into `into` of the bytes the peer sent since the last receive, at most
    /// `into.len()`, in flight: its outcome is how many, 0 once the peer has hung up. Under a
    /// byte limit ([`Net::limit`]) it asks for no more than the limit has left, takes that
    /// much from it as it starts and gives back what did not come; with nothing left the
    /// receive never starts, and its outcome is [`NetKind::OverLimit`].
    #[track_caller]
    pub fn read_into<'buf>(&'buf self, into: &'buf mut [u8]) -> Pending<'buf, ReadInto<'buf>> {
        let conn = self.link.conn;
        let peer = &self.peer;
        let budget = self.budget.as_deref();
        let mut most = into.len();
        if let Some(budget) = budget {
            most = most.min(usize::try_from(budget.left()).unwrap_or(usize::MAX));
            let short = match most {
                0 if !into.is_empty() => Some(budget.over(into.len() as u64)),
                _ => budget.take(most as u64).err(),
            };
            if let Some(short) = short {
                let over = error(NetOp::Recv, peer, NetKind::OverLimit(short));
                return pending::refused(&self.link.io, "bstd::net read_into", ReadInto::Refused(over));
            }
        }
        // alloc: the effect's buffer, which blaze-core's `Effect` owns.
        let effect = |tag| Effect::Recv { conn, most, into: Vec::new(), tag };
        pending::start(&self.link.io, "bstd::net read_into", into, effect, ReadInto::InFlight { peer, budget, most })
    }

    /// The bytes the peer sent since the last receive, at most `most`, added to `into`; how
    /// many, 0 once the peer has hung up. Receives directly via `Effect::Recv` and appends into
    /// `into` without pre-zeroing `most` bytes or making a second copy.
    pub fn recv(&self, cx: &Cx, most: usize, into: &mut Bytes<'_>) -> Result<usize, NetError> {
        let peer = &self.peer;
        let stopped = |stopped| error(NetOp::Recv, peer, NetKind::Stopped(stopped));
        cx.stopped_by(self.link.io.now()).map_err(stopped)?;
        let budget = self.budget.as_deref();
        let mut ask = most;
        if let Some(budget) = budget {
            ask = ask.min(usize::try_from(budget.left()).unwrap_or(usize::MAX));
            let short = match ask {
                0 if most > 0 => Some(budget.over(most as u64)),
                _ => budget.take(ask as u64).err(),
            };
            if let Some(short) = short {
                return Err(error(NetOp::Recv, peer, NetKind::OverLimit(short)));
            }
        }
        let conn = self.link.conn;
        let done = self.link.io.call(cx, |tag| Effect::Recv { conn, most: ask, into: Vec::new(), tag });
        match done {
            Ok(Done::Read(bytes)) if bytes.len() <= ask => {
                if let Some(budget) = budget {
                    budget.give_back((ask - bytes.len()) as u64);
                }
                let n = bytes.len();
                into.try_extend(&bytes).map_err(stopped)?;
                Ok(n)
            }
            Ok(Done::Broken { error: why, .. }) => {
                if let Some(budget) = budget {
                    budget.give_back(ask as u64);
                }
                Err(error(NetOp::Recv, peer, NetKind::Driver(why.0)))
            }
            Ok(other) => {
                if let Some(budget) = budget {
                    budget.give_back(ask as u64);
                }
                Err(unexpected(NetOp::Recv, peer, other))
            }
            Err(why) => {
                if let Some(budget) = budget {
                    budget.give_back(ask as u64);
                }
                Err(stopped(why))
            }
        }
    }

    /// The connection finished: hung up, so the peer's receives get nothing more, and the
    /// driver's word that it is, or why not, op [`NetOp::Finish`]. The one way a `Conn` ends.
    #[blaze::consumes]
    pub fn finish(mut self, cx: &Cx) -> Result<(), NetError> {
        self.unfinished.finished();
        self.link.open = false;
        let conn = self.link.conn;
        let done = self
            .link
            .io
            .call(cx, |tag| Effect::Hangup { conn, tag })
            .map_err(|stopped| error(NetOp::Finish, &self.peer, NetKind::Stopped(stopped)))?;
        match done {
            Done::HungUp => Ok(()),
            done => Err(unexpected(NetOp::Finish, &self.peer, done)),
        }
    }
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Conn({:?} to {})", self.link.conn, self.peer)
    }
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Listener({})", self.at)
    }
}

impl fmt::Debug for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Net(connect {:?}, listen {:?}, limit {:?})", self.connect, self.listen, self.budget)
    }
}
