//! Name lookup alone (docs/v2/plan/design/std.md, "The rest of the set", `Resolver`): for code
//! that should resolve and not connect. Made from a [`Net`] ([`Net::resolver`]) and never
//! reaching further than it: it resolves only the hosts that `Net` may connect to, and it has
//! no connect. Narrowed to one host by [`Resolver::only`], which never widens.
//!
//! **Provider.** A host that is an IP address resolves to itself everywhere. A DNS name
//! resolves in the simulation, from its name table ([`crate::hosted::SimHost::names`]); in a
//! real world it is refused ([`NetKind::NoLookup`]), since blaze-core has no effect that looks
//! a name up, and Rust std's resolver is not reached for in its place.
//!
//! [`Net`]: crate::net::Net
//! [`Net::resolver`]: crate::net::Net::resolver

use core::{fmt, net::SocketAddr};

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
pub(crate) fn new(io: Io, reach: net::Allowed) -> Resolver {
        Resolver { io, reach, allowed: Allowed::Any }
    }

    /// Only the host and port `name`, if this resolves it already.
    pub fn only(&self, name: &HostPort) -> Resolver {
        // clone: authority held, narrowed: a refcount or a copy.
        Resolver { io: self.io.clone(), reach: self.reach.clone(), allowed: self.allowed.one(name, self.io.plant()) }
    }

    /// The address `name` resolves to, with its port.
    pub fn lookup(&self, cx: &Cx, name: &HostPort) -> Result<SocketAddr, NetError> {
        // clone: the name as an address, a refcount.
        let addr = Addr::Tcp(name.clone());
        let refuse = |kind| net::error(NetOp::Lookup, &addr, kind);
        if !self.reach.allows(&addr) || !self.allowed.allows(name) {
            return Err(refuse(NetKind::NotAllowed));
        }
        cx.stopped_by(self.io.now()).map_err(|stopped| refuse(NetKind::Stopped(stopped)))?;
        if let Some(found) = name.socket_addr() {
            return Ok(found);
        }
        let named = self.io.script(|host| host.names.iter().find(|(it, _)| it == name.host()).map(|(_, ip)| *ip));
        match named {
            Some(Some(ip)) => Ok(SocketAddr::new(ip, name.port())),
            Some(None) => Err(refuse(NetKind::NoSuchName)),
            None => Err(refuse(NetKind::NoLookup)),
        }
    }
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Resolver(within {:?}, {:?})", self.reach, self.allowed)
    }
}
