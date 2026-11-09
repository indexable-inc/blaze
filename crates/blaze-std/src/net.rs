//! Connections (docs/v2/plan/design/std.md, `Net`): connects and listens on blaze-core's
//! sockets, narrowed by [`Net::connect_only`] and [`Net::listen_only`], which never widen.
//!
//! Addresses are socket addresses and Unix sockets beneath the driver's root: blaze-core has
//! no effect for name lookup, so `connect_only("api.stripe.com:443")` by host name needs one
//! first.

use std::{fmt, net::SocketAddr};

use blaze_core::{Address, Done, Effect, Tag};

use crate::{
    Stopped,
    cx::Cx,
    io::{Ends, Io},
    mem::Bytes,
    path::Path,
};

/// Where to connect or listen.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount or a copy
pub enum Addr {
    Tcp(SocketAddr),
    /// A Unix socket beneath the driver's root.
    Unix(Path),
}

impl Addr {
    fn address(&self) -> Address {
        match self {
            Addr::Tcp(at) => Address::Tcp(*at),
            // clone: the socket's name, a refcount.
            Addr::Unix(path) => Address::Unix(path.name().clone()),
        }
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

/// Which addresses a [`Net`] may reach one way.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount or a copy
enum Allowed {
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

    fn allows(&self, at: &Addr) -> bool {
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
    Close,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum NetKind {
    /// The capability does not reach the address (rule: narrowing never widens).
    NotAllowed,
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
            NetKind::Stopped(stopped) => write!(f, "bstd::net {op:?} to {addr}: {stopped}"),
            NetKind::Driver(said) => write!(f, "bstd::net {op:?} to {addr}: {said}"),
            NetKind::Answered(done) => {
                write!(f, "bstd::net {op:?} to {addr}: the driver answered {done} (a driver's bug)")
            }
        }
    }
}

impl std::error::Error for NetError {}

fn error(op: NetOp, addr: &Addr, kind: NetKind) -> NetError {
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
}

impl Net {
    pub(crate) fn new(io: Io) -> Net {
        Net { io, connect: Allowed::Any, listen: Allowed::Any }
    }

    /// Connects only to `at`, of what this connects to, and listens nowhere.
    pub fn connect_only(&self, at: &Addr) -> Net {
        Net { io: self.io.clone(), connect: self.connect.narrowed(at), listen: Allowed::Nothing }
    }

    /// Listens only on `at`, of what this listens on, and connects nowhere.
    pub fn listen_only(&self, at: &Addr) -> Net {
        Net { io: self.io.clone(), connect: Allowed::Nothing, listen: self.listen.narrowed(at) }
    }

    pub fn connect(&self, cx: &Cx, at: &Addr) -> Result<Conn, NetError> {
        if !self.connect.allows(at) {
            return Err(error(NetOp::Connect, at, NetKind::NotAllowed));
        }
        let address = at.address();
        let done = self
            .io
            .call(cx, |tag| Effect::Connect { at: address, tag })
            .map_err(|stopped| error(NetOp::Connect, at, NetKind::Stopped(stopped)))?;
        match done {
            // clone: the connection's peer, for its errors.
            Done::Connected { conn } => Ok(Conn { io: self.io.clone(), conn, peer: at.clone(), open: true }),
            done => Err(unexpected(NetOp::Connect, at, done)),
        }
    }

    /// Listens on `at`: each connection made to it is [`Listener::accept`]ed.
    pub fn listen(&self, cx: &Cx, at: &Addr) -> Result<Listener, NetError> {
        if !self.listen.allows(at) {
            return Err(error(NetOp::Listen, at, NetKind::NotAllowed));
        }
        cx.check().map_err(|stopped| error(NetOp::Listen, at, NetKind::Stopped(stopped)))?;
        let address = at.address();
        let tag = self.io.submit(|tag| Effect::Listen { at: address, tag });
        // clone: the listener's address, for its errors.
        Ok(Listener { io: self.io.clone(), tag, at: at.clone(), open: true })
    }
}

/// A listen in flight: dropping it ends it.
pub struct Listener {
    io: Io,
    tag: Tag,
    at: Addr,
    open: bool,
}

impl Listener {
    /// The next connection made to it.
    pub fn accept(&mut self, cx: &Cx) -> Result<Conn, NetError> {
        let done = cx
            .wait(&self.io, self.tag, Ends::Stream)
            .map_err(|stopped| error(NetOp::Accept, &self.at, NetKind::Stopped(stopped)))?;
        match done {
            // clone: the connection's address, for its errors.
            Done::Accepted { conn } => Ok(Conn { io: self.io.clone(), conn, peer: self.at.clone(), open: true }),
            done => {
                // A failed listen has ended: nothing more comes of its tag.
                self.open = false;
                Err(unexpected(NetOp::Accept, &self.at, done))
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if self.open {
            let tag = self.tag;
            self.io.forget(tag, Ends::Stream);
            let _ = self.io.submit(|_| Effect::Unlisten { tag });
        }
    }
}

/// A connection.
pub struct Conn {
    io: Io,
    conn: blaze_core::Conn,
    peer: Addr,
    open: bool,
}

impl Conn {
    pub fn peer(&self) -> &Addr {
        &self.peer
    }

    /// All of `bytes` sent.
    pub fn send(&self, cx: &Cx, bytes: &[u8]) -> Result<(), NetError> {
        let conn = self.conn;
        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
        let bytes = bytes.to_vec();
        let done = self
            .io
            .call(cx, |tag| Effect::Send { conn, bytes, tag })
            .map_err(|stopped| error(NetOp::Send, &self.peer, NetKind::Stopped(stopped)))?;
        match done {
            Done::Sent(_) => Ok(()),
            done => Err(unexpected(NetOp::Send, &self.peer, done)),
        }
    }

    /// The bytes the peer sent since the last receive, at most `most`, added to `into`; how
    /// many, 0 once the peer has hung up.
    pub fn recv(&self, cx: &Cx, most: usize, into: &mut Bytes<'_>) -> Result<usize, NetError> {
        let conn = self.conn;
        // alloc: the effect's buffer, which blaze-core's `Effect` owns.
        let buffer = Vec::new();
        let done = self
            .io
            .call(cx, |tag| Effect::Recv { conn, most, into: buffer, tag })
            .map_err(|stopped| error(NetOp::Recv, &self.peer, NetKind::Stopped(stopped)))?;
        match done {
            Done::Read(bytes) => {
                into.try_extend(&bytes)
                    .map_err(|stopped| error(NetOp::Recv, &self.peer, NetKind::Stopped(stopped)))?;
                Ok(bytes.len())
            }
            done => Err(unexpected(NetOp::Recv, &self.peer, done)),
        }
    }

    /// The connection closed: the peer's receives get nothing more.
    pub fn close(mut self, cx: &Cx) -> Result<(), NetError> {
        self.open = false;
        let conn = self.conn;
        let done = self
            .io
            .call(cx, |tag| Effect::Hangup { conn, tag })
            .map_err(|stopped| error(NetOp::Close, &self.peer, NetKind::Stopped(stopped)))?;
        match done {
            Done::HungUp => Ok(()),
            done => Err(unexpected(NetOp::Close, &self.peer, done)),
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if self.open {
            let conn = self.conn;
            let tag = self.io.submit(|tag| Effect::Hangup { conn, tag });
            self.io.forget(tag, Ends::Once);
        }
    }
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Conn({:?} to {})", self.conn, self.peer)
    }
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Listener({})", self.at)
    }
}

impl fmt::Debug for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Net(connect {:?}, listen {:?})", self.connect, self.listen)
    }
}
