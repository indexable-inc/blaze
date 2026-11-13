//! blaze's transfer packs (docs/remote-cache.md, "Packs"): how records of one machine's store
//! become another's without being built again, from a file, a directory, a static HTTP
//! address or a peer on the network, with nothing believed that was not checked.
//!
//! | module | what |
//! |---|---|
//! | [`cbor`] | the deterministic CBOR this crate writes and the bounded reader of it |
//! | [`manifest`] | [`Manifest`]: a pack's records (key, kind, length, BLAKE3), sorted by key; its bytes' BLAKE3 is the pack's name |
//! | [`head`] | [`Head`]: which manifest a pin of a scope is, and who says so (Ed25519, checked by the driver's [`Signatures`]); [`Trust`] |
//! | [`import`] | [`Import`]: a pack into a store, each record checked before it is put, the pin put before the flush |
//! | [`fetch`] | [`Fetch`]: records by key from several sources that come, fail and lie |
//! | [`peer`] | the read-only endpoint a daemon may open to listed peers: who is let in, what a request names |
//! | [`spans`] | the index of a body compressed for transfer, span by span |
//! | [`layout`] | the names of a pack's files in a directory or under a base URL |
//!
//! **What is checked, and against what.** A record's bytes against the BLAKE3 the manifest
//! gives for it (the store's own content hash, so the store checks it again as it keeps it);
//! the manifest's bytes against the pack's name; the name against a [`Head`]; the head against
//! a signature by a key the user trusts, or against the user's own word (an import of a file
//! they name, said with `--unsigned`). Per-record hashes, not a tree inside a record (Bao): a
//! record is what the store keeps and checks whole, most are a few kilobytes, and a ranged
//! read of one record is one check. A record is at most [`manifest::RECORD_MOST`] bytes, which
//! bounds what a reader holds before a check.
//!
//! **What a hash cannot say.** That the bytes are the named ones, yes. That they are an honest
//! build of what their key was hashed from, no: only whoever built them knows. So a key is
//! mapped to a record only by a manifest some trusted party vouched for ([`head`]).
//!
//! Every core here is sans-IO: a synchronous state machine that opens no file and no socket,
//! reads no clock (the fetch is told the time) and computes no hash of its own: its driver
//! hands it the hash ([`Hashes`]) and the signature check ([`Signatures`]), so its one
//! dependency is `blaze-report`, whose `say!` writes its messages.
//!
//! It is `no_std` on `core` and `alloc` alone. It is not on bstd
//! (docs/v2/plan/design/std.md) yet, by decision D25 of docs/v2/plan/decisions.json: bstd uses
//! syntax only blaze accepts, and while cargo with the pinned nightly builds and tests blaze
//! in the dev loop a crate that must be verified now cannot depend on it. When the rule
//! returns to blaze-only, its tables move to `bstd::mem::Kept` and nothing else changes.

#![cfg_attr(not(test), no_std)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(unsafe_code)]

extern crate alloc;

pub mod cbor;
pub mod fetch;
pub mod head;
pub mod import;
pub mod layout;
pub mod manifest;
pub mod peer;
pub mod spans;

#[cfg(test)]
mod tests;

use alloc::string::{String, ToString as _};
use core::fmt;

use blaze_report::{
    say,
    say::{Code, Count, Words},
};

pub use fetch::Fetch;
pub use head::{Head, Trust};
pub use import::Import;
pub use manifest::{Manifest, Record};

/// 32 bytes that name something: a store key, a record's content hash, a pack.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub [u8; 32]);

impl Id {
    /// The id `hex` spells: 64 digits, lower case.
    pub fn from_hex(hex: &str) -> Option<Id> {
        let hex = hex.as_bytes();
        if hex.len() != 64 {
            return None;
        }
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut out = [0u8; 32];
        for (at, pair) in hex.chunks_exact(2).enumerate() {
            out[at] = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        Some(Id(out))
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// The hashes a core checks with, computed by its driver: BLAKE3 in blaze
/// (`blaze_cas::content_of` for a record, a domain of its own for a manifest).
pub trait Hashes {
    /// The content hash of a record's bytes, as the store that keeps it computes it.
    fn content(&self, bytes: &[u8]) -> Id;
    /// The name of a manifest's bytes.
    fn manifest(&self, bytes: &[u8]) -> Id;
}

/// The signature check, its driver's: whether `signature` is `signer`'s over `message`
/// (Ed25519 in blaze).
pub trait Signatures {
    fn verify(&self, signer: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool;
}

/// Why something of a pack is not used. Each names the thing and the rule that refused it;
/// nothing refused is kept or read further.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The bytes are not this format's, or are of a layout this blaze does not read: `found`
    /// is what they say they are.
    Layout { what: &'static str, found: String },
    /// A length or a count past its bound, checked before anything was allocated for it.
    TooLarge { what: &'static str, len: u64, most: u64 },
    /// The bytes are not canonical CBOR of the schema, at byte `at`.
    Malformed { what: &'static str, at: usize, why: &'static str },
    /// A name that could leave its directory or is not one this format writes.
    Name { what: &'static str, name: String },
    /// A record's bytes do not hash to what the manifest says.
    Record { key: Id, index: u32, want: Id, got: Id },
    /// A record's bytes are of another length than the manifest says.
    Length { key: Id, index: u32, want: u64, got: u64 },
    /// The manifest's bytes are not the pack's.
    Manifest { want: Id, got: Id },
    /// The body ended before the record at `index` did.
    Truncated { key: Id, index: u32, at: u64, want: u64, got: u64 },
    /// The store has other bytes under the record's key (two values of one key).
    Differs { key: Id, kept: Id, given: Id },
    /// A record of a kind this import does not take.
    Kind { key: Id, kind: u8 },
    /// The head is signed by no key the user trusts, or its signature is not its signer's.
    Untrusted { what: String },
    /// The store or the body's reader failed: said as it said it.
    Driver { what: String },
    /// The driver called out of order: a bug in the driver, never a state of a pack.
    Order { what: &'static str },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Layout { what, found } => {
                write!(f, "{what} is refused: it is {found}, which this blaze does not read")
            }
            Refused::TooLarge { what, len, most } => {
                write!(f, "{what} is refused: {len} is more than the {most} it may be")
            }
            Refused::Malformed { what, at, why } => {
                write!(f, "{what} is refused at byte {at}: {why}")
            }
            // alloc: an error's text
            Refused::Name { what, name } => f.write_str(
                &say!(
                    Words(what),
                    " ",
                    Code(&name.escape_default().to_string()),
                    " is refused: a name is 1 to ",
                    Count(layout::NAME_MOST as u64),
                    " of a-z, 0-9, `.`, `_` and `-`, and starts with none of `.` and `-`"
                )
                .flat(),
            ),
            Refused::Record { key, index, want, got } => write!(
                f,
                "record {index} (key {key}) is refused: its bytes hash to {got}, and the manifest says {want}"
            ),
            Refused::Length { key, index, want, got } => write!(
                f,
                "record {index} (key {key}) is refused: it is {got} bytes, and the manifest says {want}"
            ),
            Refused::Manifest { want, got } => {
                write!(f, "the manifest is refused: its bytes hash to {got}, and the pack is {want}")
            }
            Refused::Truncated { key, index, at, want, got } => write!(
                f,
                "the body ends inside record {index} (key {key}): {got} of its {want} bytes are there, from byte {at}"
            ),
            Refused::Differs { key, kept, given } => write!(
                f,
                "record of key {key} is refused: the store keeps {kept} under that key and the pack gives {given} (two values of one key)"
            ),
            Refused::Kind { key, kind } => {
                write!(f, "record of key {key} is refused: its kind {kind} is not one this import takes")
            }
            Refused::Untrusted { what } => write!(f, "the pack is not trusted: {what}"),
            Refused::Driver { what } => f.write_str(what),
            Refused::Order { what } => write!(f, "the pack's driver called out of order: {what}"),
        }
    }
}
