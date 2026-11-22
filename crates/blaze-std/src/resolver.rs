//! Name lookup alone (docs/v2/plan/design/std.md, "The rest of the set", `Resolver`): for code
//! that should resolve and not connect. Made from a [`Net`] ([`Net::resolver`]) and never
//! reaching further than it: it resolves only the hosts that `Net` may connect to, and it has
//! no connect. Narrowed to one host by [`Resolver::only`], which never widens.
//!
//! **Provider.** A host that is an IP address resolves to itself everywhere. In a simulated
//! world, [`crate::hosted::SimHost::names`] answers first; otherwise [`blaze_core::Effect::Resolve`]
//! asks the driver (native `getaddrinfo` on the driver's host thread, or `Sim::name` in a
//! simulation).
//!
//! [`Net`]: crate::net::Net
//! [`Net::resolver`]: crate::net::Net::resolver

use alloc::{format, string::ToString, vec::Vec};
use core::{fmt, net::SocketAddr};

use blaze_core::{Done, Effect};
use blaze_mem::Frame;

use crate::{
    cx::Cx,
    io::Io,
    narrow::Allowed,
    net::{self, Addr, HostPort, NetError, NetKind, NetOp},
};

/// Name lookup: from [`crate::net::Net::resolver`].
#[derive(Clone)]
pub struct Resolver {
    io: Io,
    /// What the `Net` it came from may connect to.
    reach: net::Allowed,
    allowed: Allowed<HostPort>,
}

impl Resolver {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(io: Io, reach: net::Allowed) -> Resolver {
        Resolver { io, reach, allowed: Allowed::Any }
    }

    /// Only the host and port `name`, if this resolves it already.
    pub fn only(&self, name: &HostPort) -> Resolver {
        // clone: authority held, narrowed: a refcount or a copy.
        Resolver { io: self.io.clone(), reach: self.reach.clone(), allowed: self.allowed.one(name, self.io.plant()) }
    }

    /// The first address `name` resolves to, with its port.
    pub fn lookup(&self, cx: &Cx, name: &HostPort) -> Result<SocketAddr, NetError> {
        let mut out = Vec::new_in(cx.frame());
        self.lookup_into(cx, name, &mut out)?;
        out.first().copied().ok_or_else(|| net::error(NetOp::Lookup, &Addr::Tcp(name.clone()), NetKind::NoSuchName))
    }

    /// Every address `name` resolves to, with its port, appended into `into` (in the caller's
    /// frame).
    pub fn lookup_into(&self, cx: &Cx, name: &HostPort, into: &mut Vec<SocketAddr, &Frame>) -> Result<usize, NetError> {
        // clone: the name as an address, a refcount.
        let addr = Addr::Tcp(name.clone());
        let refuse = |kind| net::error(NetOp::Lookup, &addr, kind);
        if !self.reach.allows(&addr) || !self.allowed.allows(name) {
            return Err(refuse(NetKind::NotAllowed));
        }
        cx.stopped_by(self.io.now()).map_err(|stopped| refuse(NetKind::Stopped(stopped)))?;
        if let Some(found) = name.socket_addr() {
            into.try_reserve(1)
                .map_err(|_| refuse(NetKind::Stopped(crate::Stopped::OutOfMemory { arena: "bstd::resolver", asked: size_of::<SocketAddr>() })))?;
            into.push(found);
            return Ok(1);
        }
        // In a simulated world, `SimHost::names` overrides DNS when an entry is scripted.
        if let Some(Some(ip)) = self.io.script(|host| host.names.iter().find(|(it, _)| it == name.host()).map(|(_, ip)| *ip)) {
            into.try_reserve(1)
                .map_err(|_| refuse(NetKind::Stopped(crate::Stopped::OutOfMemory { arena: "bstd::resolver", asked: size_of::<SocketAddr>() })))?;
            into.push(SocketAddr::new(ip, name.port()));
            return Ok(1);
        }
        // Native (and simulations without a `SimHost` override): resolve through `Effect::Resolve`.
        // alloc: the effect owns the host name string.
        let host_name = name.host().to_string();
        let port = name.port();
        let done = self
            .io
            .call(cx, |tag| Effect::Resolve { name: host_name, port, tag })
            .map_err(|stopped| refuse(NetKind::Stopped(stopped)))?;
        match done {
            Done::Resolved(addrs) => {
                if addrs.is_empty() {
                    return Err(refuse(NetKind::NoSuchName));
                }
                into.try_reserve(addrs.len())
                    .map_err(|_| refuse(NetKind::Stopped(crate::Stopped::OutOfMemory { arena: "bstd::resolver", asked: addrs.len() * size_of::<SocketAddr>() })))?;
                let count = addrs.len();
                into.extend(addrs);
                Ok(count)
            }
            Done::Unresolved { .. } => Err(refuse(NetKind::NoSuchName)),
            Done::Failed(error) => Err(refuse(NetKind::Driver(error.0))),
            // alloc: an error's message.
            other => Err(refuse(NetKind::Answered(format!("{other:?}")))),
        }
    }
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Resolver(within {:?}, {:?})", self.reach, self.allowed)
    }
}
