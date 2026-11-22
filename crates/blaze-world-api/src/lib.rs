//! The world interface, version 1 (docs/v2/plan/design/world.md; row W0 of
//! docs/v2/plan/design/runtime.md): the schema and nothing behind it.
//!
//! A world is a value: the digest of a tree ([`WorldId`]), the same digest the answer's
//! certificate is keyed by. Every call names the worlds it reads and returns values; nothing
//! lives between calls (rule 70). A patch is a function from a world to a world, all or
//! nothing; a commit writes a world onto a checkout only if the checkout is still the world the
//! caller says it started from.
//!
//! | module | what |
//! |---|---|
//! | [`Request`] / [`Response`] and the types under them | the schema, its encode and its decode |
//! | [`WireError`] | every refusal of bytes: the offset, the field and the [`WireCode`] |
//! | [`Session`] | one connection, sans IO: bytes in, at most one request out; one response in, bytes out |
//! | `to_value` on [`Request`] and [`Response`] | the JSON rendering, as a [`blaze_data::Value`] |
//!
//! The encoding: every message and every record is a CBOR map keyed by small unsigned
//! integers, in ascending order, every key the schema has for that shape present and no other
//! (one value, one spelling: no optional fields, and an empty list is an empty array). A
//! message's key 0 is the schema's version and key 1 its call or reply; a variant's key 0 is
//! its tag. Integers are unsigned and at most [`MOST_INT`], so JSON and TypeScript hold them
//! exactly. No floats, tags, negative integers or nulls. The CBOR itself is blaze-cbor's
//! (deterministic: shortest heads, definite lengths), read with a cap on every length before
//! anything is sized by it ([`Limits`]). The schema has no recursive type, so its depth is
//! bounded by the schema ([`MOST_DEPTH`]).

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

mod codes;
mod dec;
mod enc;
mod error;
mod json;
mod session;
mod types;

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod vectors_tests;
#[cfg(test)]
mod wire_tests;

pub use codes::{Call, EditWhy, EntryKind, WireCode};
pub use error::{BadPath, SessionError, WireError};
pub use session::{Session, SessionFeed};
pub use types::{
    Answer, Change, Checkout, Edit, Entered, Entry, Finding, FindingId, Note, Path, Place, Refusal, Request, Response, Spot,
    Subject, Target, What, WorldId,
};

pub use blaze_id::ItemKey;

/// The schema's version: key 0 of every message. A message of another is refused by name
/// ([`WireCode::Version`]).
pub const VERSION: u64 = 1;

/// The largest integer the schema carries: 2^53 - 1, which a JSON number and a TypeScript
/// `number` hold exactly. A larger one is refused ([`WireCode::Range`]).
pub const MOST_INT: u64 = (1 << 53) - 1;

/// The deepest any message of this schema nests (a reply, its findings, a finding, its notes, a
/// note, its spots, a spot, its place): what [`Session`]'s scanner allows, refusing deeper
/// input before it is decoded.
pub const MOST_DEPTH: usize = 8;

/// The bounds a decode holds to, each checked before anything is sized by it; one over is
/// a typed refusal naming the field and the bound, never a truncation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Bytes of one message.
    pub most_bytes: usize,
    /// Bytes of one text: a file's or an item's text, a message, a why.
    pub most_text: usize,
    /// Bytes of one path, checkout, code or name.
    pub most_path: usize,
    /// Edits in one patch or one fix.
    pub most_edits: usize,
    /// Items of any other list: findings, notes, spots, changes, entries, reads, targets.
    pub most_list: usize,
}

impl Limits {
    /// 64 MiB a message, 16 MiB a text, 4 KiB a path, 4,096 edits, 65,536 items a list.
    pub const DEFAULT: Limits = Limits {
        most_bytes: 64 << 20,
        most_text: 16 << 20,
        most_path: 4096,
        most_edits: 4096,
        most_list: 1 << 16,
    };
}
