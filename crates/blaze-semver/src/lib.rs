//! Semantic versions and Cargo's version requirements, in place of the `semver` crate
//! (docs/v2/plan/third-party.md: blaze-audit, blaze-fmt, blaze-manifest, blaze-resolve and
//! blaze-source).
//!
//! - [`Version`]: `major.minor.patch[-pre][+build]` by SemVer 2.0.0, parsed strictly (no `v`,
//!   no blanks, no leading zeros in a number or a numeric pre-release identifier). Its `Ord` is
//!   precedence (section 11), then build metadata, so equal versions are exactly those that
//!   order equal; [`Version::cmp_precedence`] ignores build metadata, as the spec's order does.
//! - [`VersionReq`]: Cargo's grammar, comparators joined by `,`: `=`, `>`, `>=`, `<`, `<=`,
//!   `~`, `^`, a bare version (caret), and `*`/`x`/`X` wildcards. [`VersionReq::matches`]
//!   applies Cargo's pre-release rule: a version with a pre-release matches only when some
//!   comparator names the same `major.minor.patch` with a pre-release of its own.
//!
//! The shapes (`Op`, `Comparator { op, major, minor, patch, pre }`, `VersionReq { comparators }`)
//! are the ones the tree's callers already use from the `semver` crate, so a caller moves by
//! name; what differs is that every owner takes the allocator it may use ([`Ident`] keeps up
//! to [`INLINE`] bytes in place and allocates only past that).
//!
//! Untrusted text is bounded by [`Limits`]: the length of a text and the number of
//! comparators, each a typed refusal. Every [`Error`] names the byte offset in the text and
//! the part of the version it was reading.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::fmt;

mod data;
mod ident;
mod plant;
mod req;
mod version;

#[cfg(test)]
mod tests;

pub use ident::{BuildMetadata, INLINE, Ident, Prerelease};
pub use plant::SemverPlant;
pub use req::{Comparator, Op, VersionReq};
pub use version::Version;

/// The bounds a parse holds untrusted text to (D165: every cap a parse enforces is a field
/// here, taken as a value, and drawn by the simulation down to its least legal value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of a version's or a requirement's text ([`What::TooLong`] past it); at
    /// least 1.
    pub most_len: usize,
    /// The most comparators of a requirement ([`What::TooManyComparators`] past it); at least 1.
    pub most_comparators: usize,
}

impl Limits {
    /// What the crate ships with, for registry indexes and manifests: 1 KiB of text, 32
    /// comparators (the `semver` crate's bound on comparators).
    pub const SHIPPED: Limits = Limits { most_len: 1024, most_comparators: 32 };
    /// The least legal value of each cap: what a simulation draws them down to.
    pub const SMALLEST: Limits = Limits { most_len: 1, most_comparators: 1 };
    /// [`Limits::SHIPPED`] under the name callers outside the crate use.
    pub const CARGO: Limits = Limits::SHIPPED;
}

/// Which part of a version a parse was reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    Major,
    Minor,
    Patch,
    Pre,
    Build,
    /// Between comparators, or a requirement's operator.
    Requirement,
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Part::Major => "the major version",
            Part::Minor => "the minor version",
            Part::Patch => "the patch version",
            Part::Pre => "the pre-release",
            Part::Build => "the build metadata",
            Part::Requirement => "the requirement",
        })
    }
}

/// What is wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    /// The text ended where `Part` needed more.
    End,
    /// A number with a leading zero (`01`), which SemVer does not allow.
    LeadingZero,
    /// A number past `u64::MAX`.
    Overflow,
    /// A character that cannot stand here.
    Unexpected(char),
    /// An empty identifier: `1.0.0-`, `1.0.0-a..b`, `1.0.0+`.
    EmptyIdentifier,
    /// A requirement of more comparators than the limit.
    TooManyComparators { most: usize },
    /// A text longer than the limit.
    TooLong { len: usize, most: usize },
    /// `*` with another comparator: `*, <2`.
    WildcardNotAlone,
    /// A number after a wildcard: `1.*.3`.
    AfterWildcard,
    /// Two comparators without a comma between them: `>=1 <2`.
    ExpectedComma(char),
}

/// A text that is no version or requirement: the byte offset, the part being read, and what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub at: usize,
    pub part: Part,
    pub what: What,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Error { at, part, what } = *self;
        match what {
            What::End => write!(f, "the text ends at byte {at} while reading {part}"),
            What::LeadingZero => write!(f, "{part} at byte {at} has a leading zero"),
            What::Overflow => write!(f, "{part} at byte {at} is past {}", u64::MAX),
            What::Unexpected(ch) => write!(f, "{ch:?} at byte {at} cannot stand in {part}"),
            What::EmptyIdentifier => write!(f, "an empty identifier in {part} at byte {at}"),
            What::TooManyComparators { most } => {
                write!(f, "the requirement has more than {most} comparators (at byte {at})")
            }
            What::TooLong { len, most } => {
                write!(f, "the text is {len} bytes, past the limit of {most}")
            }
            What::WildcardNotAlone => write!(f, "`*` at byte {at} must be the only comparator"),
            What::AfterWildcard => {
                write!(f, "a number at byte {at} follows a wildcard in {part}")
            }
            What::ExpectedComma(ch) => {
                write!(f, "expected `,` between comparators at byte {at}, found {ch:?}")
            }
        }
    }
}

impl core::error::Error for Error {}

/// A cursor over the text of a version or a requirement.
struct Cursor<'t> {
    text: &'t str,
    at: usize,
}

impl<'t> Cursor<'t> {
    fn new(text: &'t str, limits: Limits) -> Result<Cursor<'t>, Error> {
        if text.len() > limits.most_len {
            return Err(Error {
                at: 0,
                part: Part::Requirement,
                what: What::TooLong { len: text.len(), most: limits.most_len },
            });
        }
        Ok(Cursor { text, at: 0 })
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn spaces(&mut self) {
        while self.peek() == Some(b' ') {
            self.at += 1;
        }
    }

    fn err(&self, part: Part, what: What) -> Error {
        Error { at: self.at, part, what }
    }

    /// The error for whatever stands at the cursor: the end, or the character there.
    fn unexpected(&self, part: Part) -> Error {
        match self.text[self.at..].chars().next() {
            None => self.err(part, What::End),
            Some(ch) => self.err(part, What::Unexpected(ch)),
        }
    }

    /// A number with no leading zero.
    fn number(&mut self, part: Part) -> Result<u64, Error> {
        let start = self.at;
        let mut value: u64 = 0;
        while let Some(b @ b'0'..=b'9') = self.peek() {
            if self.at > start && self.text.as_bytes()[start] == b'0' {
                return Err(Error { at: start, part, what: What::LeadingZero });
            }
            value = match value.checked_mul(10).and_then(|v| v.checked_add(u64::from(b - b'0'))) {
                Some(value) => value,
                None => return Err(Error { at: start, part, what: What::Overflow }),
            };
            self.at += 1;
        }
        match self.at == start {
            true => Err(self.unexpected(part)),
            false => Ok(value),
        }
    }

    /// A dotted list of identifiers of `[0-9A-Za-z-]`, as written; numeric ones of a
    /// pre-release may not have a leading zero. Stops at the first other byte.
    fn identifiers(&mut self, part: Part) -> Result<&'t str, Error> {
        let start = self.at;
        loop {
            let ident = self.at;
            while let Some(b) = self.peek() {
                if !(b.is_ascii_alphanumeric() || b == b'-') {
                    break;
                }
                self.at += 1;
            }
            let word = &self.text.as_bytes()[ident..self.at];
            if word.is_empty() {
                return Err(Error { at: ident, part, what: What::EmptyIdentifier });
            }
            let numeric = word.iter().all(u8::is_ascii_digit);
            if part == Part::Pre && numeric && word.len() > 1 && word[0] == b'0' {
                return Err(Error { at: ident, part, what: What::LeadingZero });
            }
            if !self.eat(b'.') {
                return Ok(&self.text[start..self.at]);
            }
        }
    }
}
