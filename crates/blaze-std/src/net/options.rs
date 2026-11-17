//! A TCP connect or listen with its socket options (`blaze_core::sockopt`): an explicit
//! [`TcpOptions`] value, checked against the system before any effect, so an option the
//! system lacks is refused by name ([`NetKind::Unavailable`]) unless it was asked
//! [`Ask::WhereAvailable`], and what was skipped so is said back ([`Skipped`]).
//!
//! [`Net::connect`] and [`Net::listen`] are these with [`TcpOptions::DEFAULT`] (no-delay on,
//! reuse-address on, every other option the system's): today's behaviour, unnamed.
//!
//! The system checked against is the world's: [`Platform::HOST`] for a native world, and
//! [`Platform::Simulated`] (every option, each read back as set) for a simulated one.

use blaze_core::{Done, Effect};

pub use blaze_core::sockopt::{
    Applied, Ask, Buffers, Bytes as ByteCount, Congestion, Dscp, FastOpen, KeepAlive, Linger, Opt, Platform, Pmtu,
    Seconds, Side, Skipped, Tcp, TcpOptions, UdpOptions, Unavailable, Why,
};
use blaze_core::sockopt::{check_tcp, check_udp};

use super::{Addr, Conn, Listener, Listening, Net, NetError, NetKind, NetOp, error, unexpected};
use crate::{cx::Cx, io::Io, unfinished::Unfinished};

/// The system `io`'s sockets are made on: the simulation's, or this host's.
pub(crate) fn platform(io: &Io) -> Platform {
    match io.with_sim(|_| ()) {
        Some(()) => Platform::Simulated,
        None => Platform::HOST,
    }
}

/// `options` checked for a socket of `side` on `io`'s system: what is skipped, or the
/// option refused by name, as an error of `op` at `at`.
pub(crate) fn tcp_checked(io: &Io, options: &TcpOptions, side: Side, op: NetOp, at: &Addr) -> Result<Skipped, NetError> {
    match at {
        // The default is what every driver does itself (Windows's included, which takes no
        // options), so it is never refused: today's callers keep today's behaviour.
        Addr::Tcp(_) if *options == TcpOptions::DEFAULT => return Ok(Skipped::NONE),
        Addr::Tcp(_) => {}
        // A Unix socket has none of TCP's options: only the default is taken.
        Addr::Unix(_) if *options == TcpOptions::DEFAULT => return Ok(Skipped::NONE),
        Addr::Unix(_) => return Err(error(op, at, NetKind::OptionsOnUnix)),
        Addr::Udp(_) => return Err(error(op, at, NetKind::NotStream)),
    }
    check_tcp(options, side, platform(io)).map_err(|refused| error(op, at, NetKind::Unavailable(refused)))
}

/// A datagram socket's `options` checked on `io`'s system: what is skipped, or the option
/// refused by name.
pub(crate) fn udp_checked(
    io: &Io,
    options: &blaze_core::dgram::Options,
    at: &Addr,
) -> Result<Skipped, NetError> {
    check_udp(options, platform(io)).map_err(|refused| error(NetOp::Bind, at, NetKind::Unavailable(refused)))
}

impl Net {
    /// A connection to `at` whose socket is set as `options` asks before it connects:
    /// finished with [`Conn::finish`]. Beside it, the options asked [`Ask::WhereAvailable`]
    /// that this system lacks. An option asked [`Ask::Must`] that it lacks is refused before
    /// anything is asked of the driver ([`NetKind::Unavailable`], naming the option and the
    /// system); a Unix socket takes only [`TcpOptions::DEFAULT`] ([`NetKind::OptionsOnUnix`]).
    #[track_caller]
    pub fn connect_with(&self, cx: &Cx, at: &Addr, options: &TcpOptions) -> Result<(Conn, Skipped), NetError> {
        if !self.connect.allows(at) {
            return Err(error(NetOp::Connect, at, NetKind::NotAllowed));
        }
        let skipped = tcp_checked(&self.io, options, Side::Connect, NetOp::Connect, at)?;
        let address = at.reach(NetOp::Connect)?;
        // alloc: one box, only where the options are not the default.
        let options = Tcp::new(*options);
        let done = self
            .io
            .call(cx, |tag| Effect::Connect { at: address, options, tag })
            .map_err(|stopped| error(NetOp::Connect, at, NetKind::Stopped(stopped)))?;
        match done {
            // clone: the connection's peer, for its errors; the budget shared, a refcount.
            Done::Connected { conn } => Ok((Conn::new(&self.io, conn, at.clone(), self.budget.clone()), skipped)),
            done => Err(unexpected(NetOp::Connect, at, done)),
        }
    }

    /// Listens on `at` with its socket set as `options` asks before its bind (each
    /// connection it accepts carries them): finished with [`Listener::close`]. Beside it,
    /// what was skipped, as [`Net::connect_with`] says.
    #[track_caller]
    pub fn listen_with(&self, cx: &Cx, at: &Addr, options: &TcpOptions) -> Result<(Listener, Skipped), NetError> {
        if !self.listen.allows(at) {
            return Err(error(NetOp::Listen, at, NetKind::NotAllowed));
        }
        cx.check().map_err(|stopped| error(NetOp::Listen, at, NetKind::Stopped(stopped)))?;
        let skipped = tcp_checked(&self.io, options, Side::Listen, NetOp::Listen, at)?;
        let address = at.reach(NetOp::Listen)?;
        // alloc: one box, only where the options are not the default.
        let options = Tcp::new(*options);
        let tag = self.io.submit(|tag| Effect::Listen { at: address, options, tag });
        // clone: the world, a refcount.
        let listening = Listening { io: self.io.clone(), tag, open: true };
        // clone: the listener's address, for its errors; the budget shared, a refcount.
        let listener = Listener {
            listening,
            at: at.clone(),
            budget: self.budget.clone(),
            unfinished: Unfinished::new("net::Listener", "close(cx)"),
        };
        Ok((listener, skipped))
    }
}
