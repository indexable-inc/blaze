//! A client of blaze's daemon that launches no process (docs/client.md).
//!
//! The daemon's one interface is the agent API ([`blaze_api`], rule no-mcp): envelopes of one
//! versioned schema in deterministic CBOR, framed, over its Unix socket. This crate is a client
//! of that API and nothing else: it adds no message and no second protocol.
//!
//! - [`mux`]: the sans-IO core. Requests in, frames out; the daemon's bytes in, events out;
//!   many requests under way on one connection, told apart by id.
//! - [`Client`]: the blocking driver over the socket. Every wait is to a deadline the caller
//!   passes, through blaze-core's one deadline wait.
//! - [`start`]: where the daemon is, and its start when nothing listens.
//! - [`ask`]: `build`, `check`, `test`, `why` and `status` as typed options and typed answers.
//! - [`place`]: where a command runs and what of the machine it sends with it.
//! - [`foreign`]: the same calls for a caller that is not Rust, in the API's one JSON
//!   rendering ([`blaze_api::json`]): what `blaze-client-c` and `blaze-napi` are made of.
//!
//! It links blaze-api and what that links (blaze-core, blaze-query's and blaze-vfs's types of
//! the API's bodies), and none of the compiler, the code generator or LLVM.

#![deny(unsafe_code)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod ask;
#[cfg(unix)]
mod driver;
#[cfg(unix)]
pub mod foreign;
pub mod mux;
pub mod place;
#[cfg(unix)]
pub mod start;
#[cfg(unix)]
#[expect(unsafe_code, reason = "a send with no SIGPIPE, a descriptor inherited by the daemon, the terminal's width")]
mod sys;

use std::{fmt, io, path::PathBuf};

pub use blaze_api::{ApiError, Request, Response};

pub use self::ask::{Options, Outcome, WhyOptions};
#[cfg(unix)]
pub use self::driver::{Client, Connect, Next, Waker};
pub use self::mux::{End, Event, Fault, Mux};
#[cfg(unix)]
pub use self::start::Start;

/// Why a call of the client did not give its answer.
#[derive(Debug)]
pub enum Error {
    /// The request, or the connection under it, ended with no `Done`.
    Fault(Fault),
    /// The daemon answered the request with an error, where the call's answer is one value
    /// (`why`, `status`).
    Daemon(ApiError),
    /// A connect to the socket failed for another reason than that nothing listens there.
    Connect { socket: PathBuf, error: io::Error },
    /// Nothing listens at the socket, and none was to be started.
    NoDaemon { socket: PathBuf },
    /// Nothing listens at the socket, and the daemon could not be started.
    #[cfg(unix)]
    Start(start::StartError),
    /// What was asked is no request: the option or the place it names, and why.
    Asked(String),
}

impl Error {
    /// A number a foreign caller tells the errors apart by: a [`Fault`]'s own (1 to 7), then
    /// one a variant.
    pub fn code(&self) -> u32 {
        match self {
            Error::Fault(fault) => fault.code(),
            Error::Daemon(_) => 8,
            Error::Connect { .. } => 9,
            Error::NoDaemon { .. } => 10,
            #[cfg(unix)]
            Error::Start(_) => 11,
            Error::Asked(_) => 12,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Fault(fault) => fault.fmt(f),
            Error::Daemon(error) => match &error.at {
                Some(at) => write!(f, "{} (at {at})", error.what),
                None => f.write_str(&error.what),
            },
            Error::Connect { socket, error } => {
                write!(f, "connecting to blaze's daemon at {}: {error}", socket.display())
            }
            Error::NoDaemon { socket } => {
                write!(f, "no daemon listens at {}, and none was to be started", socket.display())
            }
            #[cfg(unix)]
            Error::Start(error) => error.fmt(f),
            Error::Asked(what) => f.write_str(what),
        }
    }
}

impl std::error::Error for Error {}

impl From<Fault> for Error {
    fn from(fault: Fault) -> Error {
        Error::Fault(fault)
    }
}
