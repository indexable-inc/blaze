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

use std::{fmt, net::SocketAddr, sync::Arc};

use blaze_core::{Address, Done, Effect, Tag};

use crate::{
    Plant, Stopped,
    budget::{Budget, Over},
    cx::Cx,
    io::{Ends, Io},
    mem::Bytes,
    path::Path,
    pending::{self, Pending},
    resolver::Resolver,
};

/// Where to connect or listen.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount or a copy
pub enum Addr {
    /// A TCP host and port. Its host must be an IP address until blaze-core can look a name up.
    Tcp(HostPort),
    /// A Unix socket beneath the driver's root.
    Unix(Path),
}

impl Addr {
    /// The driver's address, or `None` for a host that would need a name lookup.
    fn address(&self) -> Option<Address> {
        match self {
            Addr::Tcp(at) => at.socket_addr().map(Address::Tcp),
            // clone: the socket's name, a refcount.
            Addr::Unix(path) => Some(Address::Unix(path.name().clone())),
        }
    }

    /// The driver's address, or the error of `op` that it needs a name lookup.
    fn reach(&self, op: NetOp) -> Result<Address, NetError> {
        self.address().ok_or_else(|| error(op, self, NetKind::NoLookup))
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Addr::Tcp(at) => write!(f, "tcp:{at}"),
            Addr::Unix(path) => write!(f, "unix:{path}"),
        }
    }
}

/// The most bytes in a host name (RFC 1035 section 2.3.4, without the root's dot).
pub const HOST_MOST: usize = 253;

/// A host and a port: a DNS name (labels of letters, digits and `-`, each 1 to 63 bytes and
/// neither starting nor ending in `-`), an IPv4 address, or an IPv6 address in brackets; then
/// a port from 1. Made by [`HostPort::new`], [`HostPort::parse`] or, for a literal,
/// [`crate::host!`] as the crate compiles.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount and a copy
pub struct HostPort {
    host: Arc<str>,
    port: u16,
}

/// What [`HostPort::new`] or [`HostPort::parse`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct HostPortError {
    /// What was given: `host:port`, or the host and port joined so.
    pub given: String,
    pub why: HostPortWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostPortWhy {
    EmptyHost,
    /// More than [`HOST_MOST`] bytes.
    HostTooLong,
    /// A label is empty, longer than 63 bytes, or starts or ends with `-`.
    BadLabel,
    /// A byte that is in no host name or address.
    BadByte,
    /// A `[` with no `]`, or an IPv6 address with a byte other than hex digits, `:` and `.`.
    BadBrackets,
    /// No `:port` after the host.
    NoPort,
    /// The port is not a number from 1 to 65535.
    BadPort,
}

impl HostPortWhy {
    const fn says(self) -> &'static str {
        match self {
            HostPortWhy::EmptyHost => "has no host",
            HostPortWhy::HostTooLong => "has a host longer than 253 bytes",
            HostPortWhy::BadLabel => {
                "has a label that is empty, longer than 63 bytes, or starts or ends with `-`"
            }
            HostPortWhy::BadByte => {
                "has a byte that is no letter, digit, `-` or `.` in its host name"
            }
            HostPortWhy::BadBrackets => {
                "has an IPv6 address that is not hex digits, `:` and `.` inside `[` and `]`"
            }
            HostPortWhy::NoPort => "has no `:port` after its host",
            HostPortWhy::BadPort => "has a port that is not a number from 1 to 65535",
        }
    }
}

impl fmt::Display for HostPortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "host and port {:?} {}", self.given, self.why.says())
    }
}

impl std::error::Error for HostPortError {}

/// Why `host` is no host, if it is not: a DNS name, an IPv4 address (which is one by these
/// rules too), or an IPv6 address in brackets. A `const fn`, shared by run time and
/// [`crate::host!`].
pub const fn refused_host(host: &str) -> Option<HostPortWhy> {
    let bytes = host.as_bytes();
    if bytes.is_empty() {
        return Some(HostPortWhy::EmptyHost);
    }
    if bytes.len() > HOST_MOST {
        return Some(HostPortWhy::HostTooLong);
    }
    if bytes[0] == b'[' {
        if bytes.len() < 3 || bytes[bytes.len() - 1] != b']' {
            return Some(HostPortWhy::BadBrackets);
        }
        let mut at = 1;
        while at < bytes.len() - 1 {
            let byte = bytes[at];
            if !(byte.is_ascii_hexdigit() || byte == b':' || byte == b'.') {
                return Some(HostPortWhy::BadBrackets);
            }
            at += 1;
        }
        return None;
    }
    // Each `.`-separated label, from `start` to the dot (or the end) at `at`.
    let (mut start, mut at) = (0, 0);
    while at <= bytes.len() {
        if at == bytes.len() || bytes[at] == b'.' {
            let len = at - start;
            if len == 0 || len > 63 || bytes[start] == b'-' || bytes[at - 1] == b'-' {
                return Some(HostPortWhy::BadLabel);
            }
            start = at + 1;
        } else if !(bytes[at].is_ascii_alphanumeric() || bytes[at] == b'-') {
            return Some(HostPortWhy::BadByte);
        }
        at += 1;
    }
    None
}

/// Where `raw` (`host:port`) splits: the index of the `:` before the port, or why there is
/// none. The port is after the last `:`, so `[::1]:443` splits after its `]`.
const fn split(raw: &str) -> Result<usize, HostPortWhy> {
    let bytes = raw.as_bytes();
    let mut at = bytes.len();
    while at > 0 {
        at -= 1;
        match bytes[at] {
            b':' => {
                // A `:` inside brackets is the address's, not the port's.
                if bytes[0] == b'[' && (at == 0 || bytes[at - 1] != b']') {
                    return Err(HostPortWhy::NoPort);
                }
                return Ok(at);
            }
            b']' => return Err(HostPortWhy::NoPort),
            _ => {}
        }
    }
    Err(HostPortWhy::NoPort)
}

/// The port `digits` spell, from 1 to 65535.
const fn port_of(digits: &[u8]) -> Result<u16, HostPortWhy> {
    if digits.is_empty() || digits.len() > 5 {
        return Err(HostPortWhy::BadPort);
    }
    let (mut port, mut at) = (0u32, 0);
    while at < digits.len() {
        if !digits[at].is_ascii_digit() {
            return Err(HostPortWhy::BadPort);
        }
        port = port * 10 + (digits[at] - b'0') as u32;
        at += 1;
    }
    if port == 0 || port > u16::MAX as u32 {
        return Err(HostPortWhy::BadPort);
    }
    Ok(port as u16)
}

/// Why `raw` (`host:port`) is none, if it is not: [`HostPort::parse`]'s rule, as a `const fn`.
pub const fn refused(raw: &str) -> Option<HostPortWhy> {
    let colon = match split(raw) {
        Ok(colon) => colon,
        Err(why) => return Some(why),
    };
    let (host, port) = raw.as_bytes().split_at(colon);
    let host = match std::str::from_utf8(host) {
        Ok(host) => host,
        Err(_) => return Some(HostPortWhy::BadByte),
    };
    if let Some(why) = refused_host(host) {
        return Some(why);
    }
    let (_, digits) = port.split_at(1);
    match port_of(digits) {
        Ok(_) => None,
        Err(why) => Some(why),
    }
}

/// [`refused`], as a compile-time panic: what [`crate::host!`] evaluates in a `const`.
pub const fn check_literal(raw: &str) {
    if let Some(why) = refused(raw) {
        panic!("{}", why.says());
    }
}

impl HostPort {
    /// `host` and `port`, checked.
    pub fn new(host: &str, port: u16) -> Result<HostPort, HostPortError> {
        let why = match refused_host(host) {
            Some(why) => Some(why),
            None if port == 0 => Some(HostPortWhy::BadPort),
            None => None,
        };
        if let Some(why) = why {
            // alloc: an error's message.
            return Err(HostPortError { given: format!("{host}:{port}"), why });
        }
        // alloc: once per name, its host.
        Ok(HostPort { host: Arc::from(host), port })
    }

    /// `host:port`, checked: `api.stripe.com:443`, `127.0.0.1:8080`, `[::1]:443`.
    pub fn parse(raw: &str) -> Result<HostPort, HostPortError> {
        // alloc: an error's message.
        let refuse = |why| HostPortError { given: raw.to_owned(), why };
        if let Some(why) = refused(raw) {
            return Err(refuse(why));
        }
        let colon = split(raw).map_err(refuse)?;
        let port = port_of(&raw.as_bytes()[colon + 1..]).map_err(refuse)?;
        // alloc: once per name, its host.
        Ok(HostPort { host: Arc::from(&raw[..colon]), port })
    }

    /// The host, an IPv6 address with its brackets.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The socket address it names when its host is an IP address; `None` for a DNS name,
    /// which needs a lookup.
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        let host = self.host();
        let host = host.strip_prefix('[').and_then(|it| it.strip_suffix(']')).unwrap_or(host);
        let ip: std::net::IpAddr = host.parse().ok()?;
        Some(SocketAddr::new(ip, self.port))
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl fmt::Debug for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HostPort({}:{})", self.host, self.port)
    }
}

impl TryFrom<&str> for HostPort {
    type Error = HostPortError;

    fn try_from(raw: &str) -> Result<HostPort, HostPortError> {
        HostPort::parse(raw)
    }
}

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
            Allowed::Only(_) | Allowed::Nothing => Allowed::Nothing,
        }
    }

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
    Send,
    Recv,
    /// [`Listener::close`].
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
    /// The host is a DNS name, and blaze-core has no effect that looks one up yet.
    NoLookup,
    /// The name service has no such name.
    NoSuchName,
    /// The `Net` it came from has a byte limit ([`Net::limit`]) with no room for it.
    OverLimit(Over),
    /// [`Listener::close`] took more than [`CLOSE_MOST`] connections made while it closed,
    /// and stopped waiting for the listen's end.
    Unending { accepted: usize },
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
            NetKind::NoLookup => write!(
                f,
                "bstd::net {op:?} to {addr}: its host is a name, and blaze-core has no effect that \
                 looks a name up yet: give its IP address"
            ),
            NetKind::NoSuchName => write!(f, "bstd::net {op:?} of {addr}: the name service has no such name"),
            NetKind::OverLimit(over) => {
                write!(f, "bstd::net {op:?} to {addr}: this Net's byte limit: {over}")
            }
            NetKind::Unending { accepted } => write!(
                f,
                "bstd::net {op:?} to {addr}: {accepted} connections were made while it closed, more \
                 than {CLOSE_MOST}, and the listen's end never came (rule bounded-progress)"
            ),
            NetKind::Stopped(stopped) => write!(f, "bstd::net {op:?} to {addr}: {stopped}"),
            NetKind::Driver(said) => write!(f, "bstd::net {op:?} to {addr}: {said}"),
            NetKind::Answered(done) => {
                write!(f, "bstd::net {op:?} to {addr}: the driver answered {done} (a driver's bug)")
            }
        }
    }
}

impl std::error::Error for NetError {}

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
    /// The bytes its connections may still send and receive, if it has a limit.
    budget: Option<Arc<Budget>>,
}

impl Net {
    pub(crate) fn new(io: Io) -> Net {
        Net { io, connect: Allowed::Any, listen: Allowed::Any, budget: None }
    }

    /// The world it is in.
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
        Net { io: self.io.clone(), connect: self.connect.narrowed(at), listen: Allowed::Nothing, budget: self.budget.clone() }
    }

    /// Listens only on `at`, of what this listens on, and connects nowhere.
    pub fn listen_only(&self, at: &Addr) -> Net {
        // clone: the budget shared, a refcount.
        Net { io: self.io.clone(), connect: Allowed::Nothing, listen: self.listen.narrowed(at), budget: self.budget.clone() }
    }

    /// The same reach with a byte limit: every connection made from the `Net` this returns,
    /// and from any narrowed from it, sends and receives at most `bytes` between them, counted
    /// by bstd. A send or receive with no room left ends in [`NetKind::OverLimit`], and
    /// nothing of it is sent. A limit set on a `Net` that has one is inside it: both count,
    /// so it never raises what is left (docs/v2/plan/design/std.md, "Budgets ride the same
    /// way"). A rate limit is not written yet.
    pub fn limit(&self, bytes: u64) -> Net {
        // clone: authority held, narrowed: refcounts or copies.
        let (connect, listen) = (self.connect.clone(), self.listen.clone());
        // clone: the budget this one is inside, a refcount.
        Net { io: self.io.clone(), connect, listen, budget: Some(Budget::new(bytes, self.budget.clone())) }
    }

    /// The bytes its connections may still send and receive; `None` with no limit.
    pub fn limit_left(&self) -> Option<u64> {
        self.budget.as_deref().map(Budget::left)
    }

    /// A connection to `at`: finished with [`Conn::finish`].
    pub fn connect(&self, cx: &Cx, at: &Addr) -> Result<Conn, NetError> {
        if !self.connect.allows(at) {
            return Err(error(NetOp::Connect, at, NetKind::NotAllowed));
        }
        let address = at.reach(NetOp::Connect)?;
        let done = self
            .io
            .call(cx, |tag| Effect::Connect { at: address, tag })
            .map_err(|stopped| error(NetOp::Connect, at, NetKind::Stopped(stopped)))?;
        match done {
            // clone: the connection's peer, for its errors.
            Done::Connected { conn } => Ok(Conn::new(&self.io, conn, at.clone(), self.budget.clone())),
            done => Err(unexpected(NetOp::Connect, at, done)),
        }
    }

    /// Listens on `at`: each connection made to it is [`Listener::accept`]ed. Finished with
    /// [`Listener::close`].
    pub fn listen(&self, cx: &Cx, at: &Addr) -> Result<Listener, NetError> {
        if !self.listen.allows(at) {
            return Err(error(NetOp::Listen, at, NetKind::NotAllowed));
        }
        cx.check().map_err(|stopped| error(NetOp::Listen, at, NetKind::Stopped(stopped)))?;
        let address = at.reach(NetOp::Listen)?;
        let tag = self.io.submit(|tag| Effect::Listen { at: address, tag });
        let listening = Listening { io: self.io.clone(), tag, open: true };
        // clone: the listener's address, for its errors; the budget shared, a refcount.
        Ok(Listener { listening, at: at.clone(), budget: self.budget.clone() })
    }
}

/// The most connections [`Listener::close`] hangs up while it waits for the listen's end,
/// before it stops waiting (rule bounded-progress).
pub const CLOSE_MOST: usize = 1024;

/// A listen in flight, finished with [`Listener::close`]. Linear (see the module's doc): a
/// panic that drops one ends the listen without waiting, and drops what comes of it.
#[blaze::linear]
#[must_use = "a listener is finished with close(cx)"]
pub struct Listener {
    listening: Listening,
    at: Addr,
    /// The byte limit of the `Net` it came from, which its connections count against.
    budget: Option<Arc<Budget>>,
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
    pub fn accept(&mut self, cx: &Cx) -> Result<Conn, NetError> {
        let listening = &mut self.listening;
        let done = cx
            .wait(&listening.io, listening.tag, Ends::Stream)
            .map_err(|stopped| error(NetOp::Accept, &self.at, NetKind::Stopped(stopped)))?;
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
    /// each connection made meanwhile, at most [`CLOSE_MOST`] of them. The one way a
    /// `Listener` ends.
    #[blaze::consumes]
    pub fn close(mut self, cx: &Cx) -> Result<(), NetError> {
        let listening = &mut self.listening;
        if !listening.open {
            return Ok(());
        }
        listening.open = false;
        let (io, tag) = (listening.io.clone(), listening.tag);
        let _ = io.submit(|_| Effect::Unlisten { tag });
        let mut accepted = 0;
        while accepted <= CLOSE_MOST {
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
        Err(error(NetOp::Close, &self.at, NetKind::Unending { accepted }))
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

impl Conn {
    fn new(io: &Io, conn: blaze_core::Conn, peer: Addr, budget: Option<Arc<Budget>>) -> Conn {
        Conn { link: Link { io: io.clone(), conn, open: true }, peer, budget }
    }

    pub fn peer(&self) -> &Addr {
        &self.peer
    }

    /// A send of all of `from`, in flight. The bytes are copied as it starts (see
    /// [`crate::pending`]), and stay lent until it ends. Under a byte limit ([`Net::limit`])
    /// the bytes are taken from it before anything is sent; with no room for them the send
    /// never starts, and its outcome is [`NetKind::OverLimit`].
    pub fn write_from<'buf>(&'buf self, from: &'buf [u8]) -> Pending<'buf, Result<(), NetError>> {
        let conn = self.link.conn;
        let peer = &self.peer;
        let asked = from.len() as u64;
        let over = move |over| error(NetOp::Send, peer, NetKind::OverLimit(over));
        // The planted bug of the budget tests: the limit is looked at once the bytes are gone.
        let after = self.link.io.plant() == Plant::BudgetCheckedAfter;
        let budget = self.budget.as_deref();
        let short = match budget {
            Some(budget) if !after => budget.take(asked).err(),
            _ => None,
        };
        if let Some(short) = short {
            return pending::refused(&self.link.io, "bstd::net write_from", Err(over(short)));
        }
        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
        let bytes = from.to_vec();
        let effect = |tag| Effect::Send { conn, bytes, tag };
        pending::start(&self.link.io, "bstd::net write_from", &mut [], effect, move |_: &mut [u8], done| {
            match done {
                Ok(Done::Sent(_)) => match budget {
                    Some(budget) if after => budget.take(asked).map_err(over),
                    _ => Ok(()),
                },
                Ok(done) => Err(unexpected(NetOp::Send, peer, done)),
                Err(stopped) => Err(error(NetOp::Send, peer, NetKind::Stopped(stopped))),
            }
        })
    }

    /// All of `bytes` sent: its [`Conn::write_from`], waited for.
    pub fn send(&self, cx: &Cx, bytes: &[u8]) -> Result<(), NetError> {
        cx.stopped_by(self.link.io.now())
            .map_err(|stopped| error(NetOp::Send, &self.peer, NetKind::Stopped(stopped)))?;
        self.write_from(bytes).wait(cx)
    }

    /// A receive into `into` of the bytes the peer sent since the last receive, at most
    /// `into.len()`, in flight: its outcome is how many, 0 once the peer has hung up. Under a
    /// byte limit ([`Net::limit`]) it asks for no more than the limit has left, takes that
    /// much from it as it starts and gives back what did not come; with nothing left the
    /// receive never starts, and its outcome is [`NetKind::OverLimit`].
    pub fn read_into<'buf>(&'buf self, into: &'buf mut [u8]) -> Pending<'buf, Result<usize, NetError>> {
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
                return pending::refused(&self.link.io, "bstd::net read_into", Err(over));
            }
        }
        // What was taken from the limit and did not come is given back.
        let unused = move |came: usize| {
            if let Some(budget) = budget {
                budget.give_back((most - came) as u64);
            }
        };
        // alloc: the effect's buffer, which blaze-core's `Effect` owns.
        let effect = |tag| Effect::Recv { conn, most, into: Vec::new(), tag };
        pending::start(&self.link.io, "bstd::net read_into", into, effect, move |into: &mut [u8], done| {
            match done {
                Ok(Done::Read(bytes)) if bytes.len() <= most => {
                    into[..bytes.len()].copy_from_slice(&bytes);
                    unused(bytes.len());
                    Ok(bytes.len())
                }
                Ok(done) => {
                    unused(0);
                    Err(unexpected(NetOp::Recv, peer, done))
                }
                Err(stopped) => {
                    unused(0);
                    Err(error(NetOp::Recv, peer, NetKind::Stopped(stopped)))
                }
            }
        })
    }

    /// The bytes the peer sent since the last receive, at most `most`, added to `into`; how
    /// many, 0 once the peer has hung up. Its [`Conn::read_into`], waited for.
    pub fn recv(&self, cx: &Cx, most: usize, into: &mut Bytes<'_>) -> Result<usize, NetError> {
        let stopped = |stopped| error(NetOp::Recv, &self.peer, NetKind::Stopped(stopped));
        cx.stopped_by(self.link.io.now()).map_err(stopped)?;
        let start = into.len();
        let slot = into.grow_zeroed(most).map_err(stopped)?;
        let read = self.read_into(slot).wait(cx);
        match read {
            Ok(read) => {
                into.truncate(start + read);
                Ok(read)
            }
            Err(error) => {
                into.truncate(start);
                Err(error)
            }
        }
    }

    /// The connection finished: hung up, so the peer's receives get nothing more, and the
    /// driver's word that it is, or why not, op [`NetOp::Finish`]. The one way a `Conn` ends.
    #[blaze::consumes]
    pub fn finish(mut self, cx: &Cx) -> Result<(), NetError> {
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
