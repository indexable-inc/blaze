//! The typed refusals of this crate.

use core::fmt;

use blaze_own::{AString, Alloc};

use crate::{Refusal, WireCode};

/// Bytes refused: where, what was wrong, which field of the schema was being read, and the
/// number the code is about (the key, the tag, the version or the limit; 0 when none).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WireError {
    pub at: usize,
    pub code: WireCode,
    pub field: &'static str,
    pub said: u64,
    /// blaze-cbor's reason, for [`WireCode::Cbor`] and the codes it gives; empty otherwise.
    pub cbor: &'static str,
}

impl WireError {
    pub(crate) fn new(at: usize, code: WireCode, field: &'static str, said: u64) -> WireError {
        WireError { at, code, field, said, cbor: "" }
    }

    /// The refusal a server sends back for it.
    pub fn refusal<'a>(&self, alloc: Alloc<'a>) -> Refusal<'a> {
        Refusal::Wire {
            at: crate::enc::wide(self.at),
            what: self.code,
            field: AString::from_str_in(self.field, alloc),
            said: self.said,
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "world schema {}: byte {}, field `{}`: {}", crate::VERSION, self.at, self.field, self.code.name())?;
        match self.code {
            WireCode::UnknownKey | WireCode::DuplicateKey | WireCode::KeyOrder | WireCode::MissingKey => {
                write!(f, " (key {})", self.said)?;
            }
            WireCode::UnknownVariant => write!(f, " (tag {})", self.said)?,
            WireCode::Version => write!(f, " (version {}; this side speaks {})", self.said, crate::VERSION)?,
            WireCode::TooLong | WireCode::TooMany | WireCode::TooLarge | WireCode::TooDeep => {
                write!(f, " (the limit is {})", self.said)?;
            }
            _ => {}
        }
        match self.cbor.is_empty() {
            true => Ok(()),
            false => write!(f, ": {}", self.cbor),
        }
    }
}

impl core::error::Error for WireError {}

/// Text that is not a [`crate::Path`] (or not a [`crate::Checkout`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BadPath;

impl fmt::Display for BadPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "not a world path (relative, `/`-separated, no empty, `.` or `..` part, no NUL or `\\`) or not an absolute checkout",
        )
    }
}

impl core::error::Error for BadPath {}

/// A [`crate::Session`] driven out of its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionError {
    /// A response before a request was read or refused.
    NotAsked,
    /// A second response on one connection.
    Answered,
    /// Bytes fed after the session answered.
    Closed,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SessionError::NotAsked => "a world session answered before its request was read or refused",
            SessionError::Answered => "a world session answered twice: one response a connection",
            SessionError::Closed => "bytes fed to a world session that has answered",
        })
    }
}

impl core::error::Error for SessionError {}
