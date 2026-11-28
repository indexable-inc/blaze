//! Typed linear network requests (`Listen`, `Accept`, `Connect`, `SendFrom`, `RecvInto`,
//! `CloseConn`, `Unlisten`) and [`NetError`] with `#[blaze::args]` `*Opts` structs (rule 83).

use core::fmt;
use core::marker::PhantomData;

use blaze_machine::Request;
use blaze_own::Brand;

use crate::Address;
use crate::handle::{Conn as ConnHandle, ConnId, HandleError, Listener, ListenerId};
use crate::request::{BufError, DmaBuf, FilledBuf};

/// Typed stream networking error naming the failing endpoint or handle.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NetError {
    ConnectionRefused { addr: Address },
    AddressInUse { addr: Address },
    BrokenPipe { message: std::string::String },
    Closed,
    StaleHandle(HandleError),
    Io { addr: Option<Address>, message: std::string::String },
}

impl NetError {
    pub(crate) fn from_io(addr: Option<Address>, err: crate::IoError) -> Self {
        let msg = err.to_string();
        if msg.contains("connection refused") || msg.contains("Connection refused") {
            if let Some(addr) = addr {
                return NetError::ConnectionRefused { addr };
            }
        }
        if msg.contains("another listen has it") || msg.contains("already in use") {
            if let Some(addr) = addr {
                return NetError::AddressInUse { addr };
            }
        }
        if msg.contains("broken pipe") || msg.contains("hung up") {
            return NetError::BrokenPipe { message: msg };
        }
        NetError::Io { addr, message: msg }
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::ConnectionRefused { addr } => write!(f, "connection refused: {addr}"),
            NetError::AddressInUse { addr } => write!(f, "address already in use: {addr}"),
            NetError::BrokenPipe { message } => write!(f, "broken pipe: {message}"),
            NetError::Closed => f.write_str("listener or connection closed"),
            NetError::StaleHandle(e) => write!(f, "stale handle: {e}"),
            NetError::Io { addr: Some(a), message } => write!(f, "network error on {a}: {message}"),
            NetError::Io { addr: None, message } => write!(f, "network error: {message}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Options structs (`#[blaze::args]`, rule 83)
// ---------------------------------------------------------------------------

/// Options for binding a stream listener (`Effect::Listen`).
#[blaze::args]
pub struct ListenOpts {
    pub addr: Address,
}

/// Options for connecting a stream socket (`Effect::Connect`).
#[blaze::args]
pub struct ConnectOpts {
    pub addr: Address,
}

/// Options for sending caller-owned DMA bytes over a connection (`Effect::Send`).
#[blaze::args]
pub struct SendFromOpts<'a> {
    pub buf: DmaBuf<'a>,
}

/// Options for receiving bytes into a caller-owned DMA buffer (`Effect::Recv`).
#[blaze::args]
pub struct RecvIntoOpts<'a> {
    pub buf: DmaBuf<'a>,
}

// ---------------------------------------------------------------------------
// Typed linear network requests implementing `blaze_machine::Request`
// ---------------------------------------------------------------------------

#[blaze::linear]
#[derive(Debug)]
pub struct Listen<'io> {
    pub(crate) addr: Address,
    pub(crate) _brand: PhantomData<Brand<'io>>,
}

impl<'io> Listen<'io> {
    #[inline]
    pub fn new(opts: ListenOpts) -> Self {
        Self { addr: opts.addr, _brand: PhantomData }
    }
}

impl<'io> Request for Listen<'io> {
    type Answer<'s> = Result<Listener<'io>, NetError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Accept<'io> {
    pub(crate) listener: ListenerId<'io>,
}

impl<'io> Request for Accept<'io> {
    type Answer<'s> = Result<ConnHandle<'io>, NetError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Connect<'io> {
    pub(crate) addr: Address,
    pub(crate) _brand: PhantomData<Brand<'io>>,
}

impl<'io> Connect<'io> {
    #[inline]
    pub fn new(opts: ConnectOpts) -> Self {
        Self { addr: opts.addr, _brand: PhantomData }
    }
}

impl<'io> Request for Connect<'io> {
    type Answer<'s> = Result<ConnHandle<'io>, NetError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct SendFrom<'io, 'a> {
    pub(crate) conn: ConnId<'io>,
    pub(crate) buf: DmaBuf<'a>,
}

impl<'io, 'a> Request for SendFrom<'io, 'a> {
    type Answer<'s> = Result<DmaBuf<'a>, BufError<'a, NetError>>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct RecvInto<'io, 'a> {
    pub(crate) conn: ConnId<'io>,
    pub(crate) buf: DmaBuf<'a>,
}

impl<'io, 'a> Request for RecvInto<'io, 'a> {
    type Answer<'s> = Result<FilledBuf<'a>, BufError<'a, NetError>>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct CloseConn<'io> {
    pub(crate) conn: ConnId<'io>,
}

impl<'io> Request for CloseConn<'io> {
    type Answer<'s> = Result<(), NetError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Unlisten<'io> {
    pub(crate) listener: ListenerId<'io>,
}

impl<'io> Request for Unlisten<'io> {
    type Answer<'s> = Result<(), NetError>;
}

// ---------------------------------------------------------------------------
// Handle methods building inert network requests
// ---------------------------------------------------------------------------

impl<'io> Listener<'io> {
    #[inline]
    pub fn accept(&self) -> Accept<'io> {
        Accept { listener: self.id() }
    }

    /// Consumes this linear listener handle into an [`Unlisten`] request.
    #[blaze::consumes]
    #[inline]
    pub fn unlisten(self) -> Unlisten<'io> {
        Unlisten { listener: self.into_id() }
    }
}

impl<'io> ConnHandle<'io> {
    #[inline]
    pub fn send_from<'a>(&self, opts: SendFromOpts<'a>) -> SendFrom<'io, 'a> {
        SendFrom { conn: self.id(), buf: opts.buf }
    }

    #[inline]
    pub fn recv_into<'a>(&self, opts: RecvIntoOpts<'a>) -> RecvInto<'io, 'a> {
        RecvInto { conn: self.id(), buf: opts.buf }
    }

    /// Consumes this linear connection handle into a [`CloseConn`] request.
    #[blaze::consumes]
    #[inline]
    pub fn close(self) -> CloseConn<'io> {
        CloseConn { conn: self.into_id() }
    }
}
