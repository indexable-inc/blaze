//! What a test result asks (docs/v2/plan/design/test-selection.md, section 3): the shape of a
//! `TestRun` outside record, and the value filed at its address.
//!
//! - An [`Ask`] is one thing a run's result depends on: a function it executed, by identity
//!   ([`FnId`]; its answer is the function's [`crate::table::Bound`] now, [`ABSENT`] when gone),
//!   [`UNLOGGED`] (every function not called through a slot; its answer is
//!   [`crate::table::Image::unlogged`]), or an outside read by name (a file, a directory
//!   listing, an environment variable; its answer is the digest of what is there now,
//!   [`ABSENT`] when nothing is).
//! - A shape ([`write_shape`], read in place by [`Shape`]) is the sorted asks of one run, each
//!   once, with the basis: what the run's execution record was. Canonical: equal asks are equal
//!   bytes, so a run that asks what an earlier one asked finds the earlier one's slot.
//! - The value ([`write_value_payload`]) is a pass and its basis, and nothing else: a function of
//!   its address, so filing it twice is the same bytes.
//!
//! Costs: encoding a shape is O(asks log asks) for the sort; reading one is O(1) to open and
//! O(1) per ask, in place; an ask's id is one hash of its bytes.

#![blaze::sans_io]

use core::fmt;
use std::alloc::Allocator;

use blaze_ids::{ContentId, Hasher};

use crate::table::FnId;

/// The answer of an ask whose subject is not there now (a gone function, an absent file or
/// variable): absence is a value (rule keys-cover-every-input).
pub const ABSENT: ContentId = ContentId([0; 32]);

/// The pseudo-function that stands for every function not called through a slot (a function
/// called only directly, the premapped dependencies): in every shape, so a change to any of them
/// reruns every test.
pub const UNLOGGED: FnId = FnId(ContentId([0xff; 32]));

/// The domain of an ask's id.
pub const ASK_DOMAIN: &str = "blaze-testsel ask 1";

const SHAPE_MAGIC: &[u8; 4] = b"BZTS";
const VALUE_MAGIC: &[u8; 4] = b"BZTV";
const VERSION: u8 = 1;
/// The magic, the version, the basis and the count.
const SHAPE_HEAD: usize = 4 + 1 + 1 + 4;
/// What a passing result's value says.
const PASSED: u8 = 1;

/// What a kept result's function asks are: what the run executed, or, when its execution record
/// was not whole, what it reaches (a superset, so still sound, less exact).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Basis {
    Executed = 0,
    Reached = 1,
}

impl Basis {
    pub fn of_byte(byte: u8) -> Option<Basis> {
        match byte {
            0 => Some(Basis::Executed),
            1 => Some(Basis::Reached),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Basis::Executed => "executed",
            Basis::Reached => "reached",
        }
    }
}

/// What an outside read read.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum ReadKind {
    /// A file's bytes (or its absence), by path as the process named it.
    File = 1,
    /// A directory's sorted names.
    Dir = 2,
    /// An environment variable's value (or its absence).
    Env = 3,
}

impl ReadKind {
    fn of_byte(byte: u8) -> Option<ReadKind> {
        match byte {
            1 => Some(ReadKind::File),
            2 => Some(ReadKind::Dir),
            3 => Some(ReadKind::Env),
            _ => None,
        }
    }
}

/// One ask. The order (functions first, by identity; then reads, by kind and name) is the
/// shape's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Ask<'a> {
    Function(FnId),
    Read(ReadKind, &'a [u8]),
}

impl Ask<'_> {
    /// The ask's id: what the postings and a value's facts name it by.
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new(ASK_DOMAIN);
        match self {
            Ask::Function(function) => {
                hasher.update(&[0]).id(&function.0);
            }
            Ask::Read(kind, name) => {
                hasher.update(&[*kind as u8]).part(name);
            }
        }
        hasher.finish()
    }

    pub fn is_read(&self) -> bool {
        matches!(self, Ask::Read(..))
    }

    fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match self {
            Ask::Function(function) => {
                out.push(0);
                out.extend_from_slice(&function.0.0);
            }
            Ask::Read(kind, name) => {
                out.push(*kind as u8);
                out.extend_from_slice(&(name.len() as u32).to_le_bytes());
                out.extend_from_slice(name);
            }
        }
    }
}

/// Why bytes are not a shape: where, and what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShapeError {
    pub at: u32,
    pub what: &'static str,
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a test shape that does not decode at byte {}: {}", self.at, self.what)
    }
}

/// Writes the shape of `asks` with `basis` into `out` (cleared first). `asks` is sorted and
/// each once (the caller's sort; checked in debug builds).
pub fn write_shape<A: Allocator>(basis: Basis, asks: &[Ask<'_>], out: &mut Vec<u8, A>) {
    debug_assert!(asks.windows(2).all(|pair| pair[0] < pair[1]), "a shape's asks are sorted, each once");
    out.clear();
    out.extend_from_slice(SHAPE_MAGIC);
    out.push(VERSION);
    out.push(basis as u8);
    out.extend_from_slice(&(asks.len() as u32).to_le_bytes());
    for ask in asks {
        ask.encode(out);
    }
}

/// A shape read in place.
#[derive(Clone, Copy, Debug)]
pub struct Shape<'b> {
    basis: Basis,
    count: u32,
    asks: &'b [u8],
}

impl<'b> Shape<'b> {
    /// Opens `bytes` as a shape, checking every ask decodes and the asks are sorted, each once:
    /// O(asks). After it, [`Shape::asks`] cannot fail.
    pub fn decode(bytes: &'b [u8]) -> Result<Shape<'b>, ShapeError> {
        if bytes.len() < SHAPE_HEAD {
            return Err(ShapeError { at: 0, what: "shorter than a shape's head" });
        }
        if &bytes[0..4] != SHAPE_MAGIC {
            return Err(ShapeError { at: 0, what: "not a test shape's magic" });
        }
        if bytes[4] != VERSION {
            return Err(ShapeError { at: 4, what: "a shape of another version" });
        }
        let Some(basis) = Basis::of_byte(bytes[5]) else {
            return Err(ShapeError { at: 5, what: "a basis no basis has" });
        };
        let count = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
        let shape = Shape { basis, count, asks: &bytes[SHAPE_HEAD..] };
        let mut rest = shape.asks;
        let mut last: Option<Ask<'b>> = None;
        for _ in 0..count {
            let at = (bytes.len() - rest.len()) as u32;
            let Some((ask, after)) = take_ask(rest) else {
                return Err(ShapeError { at, what: "an ask that does not decode" });
            };
            if last.is_some_and(|last| last >= ask) {
                return Err(ShapeError { at, what: "asks out of order or twice" });
            }
            last = Some(ask);
            rest = after;
        }
        if !rest.is_empty() {
            return Err(ShapeError { at: (bytes.len() - rest.len()) as u32, what: "bytes after the last ask" });
        }
        Ok(shape)
    }

    /// `bytes`, which a [`Shape::decode`] already checked (a test's own shape, an index's read
    /// table), opened again in O(1): its head only.
    pub fn checked(bytes: &'b [u8]) -> Shape<'b> {
        debug_assert!(Shape::decode(bytes).is_ok(), "a shape opened unchecked that does not decode");
        if bytes.len() < SHAPE_HEAD {
            return Shape { basis: Basis::Executed, count: 0, asks: &[] };
        }
        Shape {
            basis: Basis::of_byte(bytes[5]).unwrap_or(Basis::Reached),
            count: u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
            asks: &bytes[SHAPE_HEAD..],
        }
    }

    pub fn basis(&self) -> Basis {
        self.basis
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The asks, in order.
    pub fn asks(&self) -> Asks<'b> {
        Asks { rest: self.asks, left: self.count }
    }
}

/// The asks of a [`Shape`], in order.
#[derive(Clone)]
pub struct Asks<'b> {
    rest: &'b [u8],
    left: u32,
}

impl<'b> Iterator for Asks<'b> {
    type Item = Ask<'b>;

    fn next(&mut self) -> Option<Ask<'b>> {
        if self.left == 0 {
            return None;
        }
        let (ask, rest) = take_ask(self.rest)?;
        self.rest = rest;
        self.left -= 1;
        Some(ask)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}

fn take_ask(bytes: &[u8]) -> Option<(Ask<'_>, &[u8])> {
    let (&tag, rest) = bytes.split_first()?;
    if tag == 0 {
        let id = rest.get(..32)?;
        let mut function = [0; 32];
        function.copy_from_slice(id);
        return Some((Ask::Function(FnId(ContentId(function))), &rest[32..]));
    }
    let kind = ReadKind::of_byte(tag)?;
    let len = rest.get(..4)?;
    let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
    let name = rest.get(4..4 + len)?;
    Some((Ask::Read(kind, name), &rest[4 + len..]))
}

/// The payload of a kept result's value: a pass, under `basis`. Four bytes of magic, the version,
/// the outcome and the basis: equal for every pass of one address.
pub fn write_value_payload(basis: Basis) -> [u8; 7] {
    let mut out = [0; 7];
    out[0..4].copy_from_slice(VALUE_MAGIC);
    out[4] = VERSION;
    out[5] = PASSED;
    out[6] = basis as u8;
    out
}

/// The basis of a value payload that says a pass; `None` for bytes that are not one.
pub fn read_value_payload(payload: &[u8]) -> Option<Basis> {
    if payload.len() != 7 || &payload[0..4] != VALUE_MAGIC || payload[4] != VERSION || payload[5] != PASSED {
        return None;
    }
    Basis::of_byte(payload[6])
}
