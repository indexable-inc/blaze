//! Socket option checking and re-exports (`blaze_core::sockopt`).
//!
//! The system checked against is the world's: [`Platform::HOST`] for a native world, and
//! [`Platform::Simulated`] (every option, each read back as set) for a simulated one.

pub use blaze_core::sockopt::{
    Applied, Ask, Buffers, Bytes as ByteCount, Congestion, Dscp, FastOpen, KeepAlive, Linger, MulticastV4, Opt,
    Platform, Pmtu, Seconds, Side, Skipped, Tcp, TcpOptions, UdpOptions, Unavailable, Why,
};
use blaze_core::sockopt::check_udp;

use super::{Addr, NetError, NetKind, NetOp, error};
use crate::io::Io;

/// The system `io`'s sockets are made on: the simulation's, or this host's.
pub(crate) fn platform(io: &Io) -> Platform {
    match io.with_sim(|_| ()) {
        Some(()) => Platform::Simulated,
        None => Platform::HOST,
    }
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
