//! External generation: a run whose generation phase takes each test case
//! from a [`Source`] instead of drawing it at random.
//!
//! A test case is described by its *choices*: the sequence of primitive
//! values (integers, booleans, floats, byte strings, strings) the engine
//! hands out as the test draws, in order. A [`Proposal`] names a case as a
//! choice prefix plus a [`Tail`] that says how to draw past it, and the
//! engine turns it into a test case, runs it, and reports what ran as a
//! [`Report`]: each realized choice with its kind and bounds, the spans the
//! draws formed, and the status.
//!
//! ```no_run
//! use hegel::explore::{Choice, Next, Proposal, Report, Source, Tail};
//! use hegel::generators as gs;
//!
//! struct Seeds(u64);
//!
//! impl Source for Seeds {
//!     fn next(&mut self) -> Next {
//!         if self.0 == 100 {
//!             return Next::Done;
//!         }
//!         self.0 += 1;
//!         Next::Run(Proposal {
//!             id: self.0,
//!             prefix: vec![Choice::Integer(0.into())],
//!             tail: Tail::Random { seed: self.0 },
//!         })
//!     }
//!
//!     fn observe(&mut self, report: &Report) {
//!         println!("case {} drew {:?}", report.id, report.choices());
//!     }
//! }
//!
//! hegel::Hegel::new(|tc| {
//!     let a: i64 = tc.draw(gs::integers());
//!     let b: i64 = tc.draw(gs::integers());
//!     assert!(a.wrapping_add(b) >= a || b < 0);
//! })
//! .run_with_source(&mut Seeds(0));
//! ```
//!
//! Each draw takes the next choice of the prefix. A choice that does not
//! fit its draw (another type, or out of the draw's bounds) is replaced by
//! a value of the draw's that does; the report shows what was used. Past
//! the prefix, a [`Tail::Random`] tail draws from a generator seeded with
//! its seed alone, so the same proposal runs the same case whatever ran
//! before it, and a [`Tail::Simplest`] tail draws every value at its
//! simplest.
//!
//! Everything outside the generation phase runs as it does for
//! [`Hegel::run`](crate::Hegel::run): failures saved in the database replay
//! first, every failure found is shrunk and saved, and the run panics with
//! the shrunk failure. The engine's own exploration (targeting and
//! mutation) does not run. Generation ends at [`Next::Done`], or where it
//! would anyway: at [`test_cases`](crate::Settings::test_cases) cases, or
//! after a failure when [`stop_on_first_failure`] is set. Health checks
//! apply to proposed cases too: a source whose cases the test mostly
//! rejects fails the run with
//! [`FilterTooMuch`](crate::HealthCheck::FilterTooMuch) unless that check
//! is suppressed.
//!
//! [`stop_on_first_failure`]: crate::Settings::stop_on_first_failure

use crate::control::hegel_internal_error;

/// Where a run under external generation takes its test cases from.
///
/// The run calls [`next`](Self::next) whenever it wants its next
/// generation case, and [`observe`](Self::observe) with each proposed
/// case's report after the case runs, before it calls `next` again.
pub trait Source {
    /// The next case to run, or [`Next::Done`] to end generation.
    fn next(&mut self) -> Next;

    /// The report of the case the proposal with `report.id` described.
    fn observe(&mut self, report: &Report);
}

/// A [`Source`]'s answer to a request for the next case.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// Run this case.
    Run(Proposal),
    /// End generation. The run goes on to shrink what it found.
    Done,
}

/// One test case, as data.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    /// Echoed as the [`Report::id`] of the case's report.
    pub id: u64,
    /// The case's first choices, taken by its draws in order.
    pub prefix: Vec<Choice>,
    /// How the case draws once the prefix runs out.
    pub tail: Tail,
}

/// How a proposed case draws past its prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    /// At random, from a generator seeded with `seed`.
    Random {
        /// The generator's seed.
        seed: u64,
    },
    /// Every draw at its simplest value.
    Simplest,
}

/// One choice: the value a single draw produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    /// An integer draw.
    Integer(Integer),
    /// A boolean draw.
    Boolean(bool),
    /// A float draw.
    Float(f64),
    /// A byte-string draw.
    Bytes(Vec<u8>),
    /// A string draw, as its codepoints.
    String(Vec<u32>),
    /// The choices of a cloned test case.
    Clone(Vec<Choice>),
}

/// An integer choice: any value of `-u128::MAX..=u128::MAX`, which covers
/// every integer type a generator draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Integer {
    negative: bool,
    magnitude: u128,
}

impl Integer {
    /// The value as an `i128`, if it fits.
    pub fn to_i128(self) -> Option<i128> {
        if self.negative {
            0i128.checked_sub_unsigned(self.magnitude)
        } else {
            i128::try_from(self.magnitude).ok()
        }
    }

    /// The value as a `u128`, if it fits.
    pub fn to_u128(self) -> Option<u128> {
        (!self.negative).then_some(self.magnitude)
    }

    fn from_magnitude(negative: bool, magnitude: u128) -> Self {
        Integer {
            negative: negative && magnitude != 0,
            magnitude,
        }
    }

    fn to_le_bytes(self) -> [u8; 17] {
        let mut out = [0u8; 17];
        if self.negative {
            out[..16].copy_from_slice(&self.magnitude.wrapping_neg().to_le_bytes());
            out[16] = 0xff;
        } else {
            out[..16].copy_from_slice(&self.magnitude.to_le_bytes());
        }
        out
    }

    fn from_le_bytes(bytes: &[u8]) -> Option<Self> {
        let negative = bytes.last().is_some_and(|b| b & 0x80 != 0);
        let fill = if negative { 0xff } else { 0 };
        let mut wide = [fill; 17];
        let (low, high) = bytes.split_at(bytes.len().min(17));
        if high.iter().any(|&b| b != fill) {
            return None;
        }
        wide[..low.len()].copy_from_slice(low);
        if wide[16] != fill {
            return None;
        }
        let mut low = [0u8; 16];
        low.copy_from_slice(&wide[..16]);
        let low = u128::from_le_bytes(low);
        if !negative {
            return Some(Integer::from_magnitude(false, low));
        }
        (low != 0).then(|| Integer::from_magnitude(true, low.wrapping_neg()))
    }
}

macro_rules! integer_from {
    (unsigned: $($t:ty),*; signed: $($s:ty),*) => {
        $(impl From<$t> for Integer {
            fn from(value: $t) -> Self {
                Integer::from_magnitude(false, value as u128)
            }
        })*
        $(impl From<$s> for Integer {
            fn from(value: $s) -> Self {
                Integer::from_magnitude(value < 0, value.unsigned_abs() as u128)
            }
        })*
    };
}

integer_from!(unsigned: u8, u16, u32, u64, u128, usize; signed: i8, i16, i32, i64, i128, isize);

impl Ord for Integer {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match (self.negative, other.negative) {
            (false, false) => self.magnitude.cmp(&other.magnitude),
            (true, true) => other.magnitude.cmp(&self.magnitude),
            (negative, _) => other.negative.cmp(&negative),
        }
    }
}

impl PartialOrd for Integer {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl core::fmt::Display for Integer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.negative {
            write!(f, "-")?;
        }
        write!(f, "{}", self.magnitude)
    }
}

/// What ran for one proposed case.
///
/// Equality compares floats as `f64` does, so a report holding a NaN is
/// never equal to itself; compare [`f64::to_bits`] where that matters.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The [`Proposal::id`] of the proposal the case was built from.
    pub id: u64,
    /// How the case ended.
    pub status: Status,
    /// For an [`Interesting`](Status::Interesting) case, the failure's
    /// origin: the test's panic location, which tells distinct failures
    /// apart.
    pub origin: Option<String>,
    /// Every draw the case made, in order.
    pub nodes: Vec<Node>,
    /// The spans the draws formed, in the order they opened.
    pub spans: Vec<Span>,
    /// The scores the test recorded with `tc.target`, sorted by label.
    pub targets: Vec<(String, f64)>,
    /// The events the test recorded with `tc.event`, in order.
    pub events: Vec<(String, Option<f64>)>,
}

impl Report {
    /// The case's choices: a prefix that replays exactly this case.
    pub fn choices(&self) -> Vec<Choice> {
        self.nodes.iter().map(Node::choice).collect()
    }
}

/// How a test case ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// The test passed.
    Valid,
    /// The test rejected the case (`tc.assume(false)`, a filter that gave up).
    Invalid,
    /// The case ran out of choices before the test finished.
    Overrun,
    /// The test failed.
    Interesting,
}

/// One draw a test case made.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Whether the generator fixed the value instead of drawing it, so no
    /// prefix can change it.
    pub forced: bool,
    /// The draw's kind, bounds and value.
    pub draw: Draw,
}

impl Node {
    /// The value this draw produced.
    pub fn choice(&self) -> Choice {
        match &self.draw {
            Draw::Integer { value, .. } => Choice::Integer(*value),
            Draw::Boolean { value, .. } => Choice::Boolean(*value),
            Draw::Float { value, .. } => Choice::Float(*value),
            Draw::Bytes { value, .. } => Choice::Bytes(value.clone()),
            Draw::String { value, .. } => Choice::String(value.clone()),
            Draw::Clone { nodes, .. } => Choice::Clone(nodes.iter().map(Node::choice).collect()),
        }
    }
}

/// A draw's kind, bounds and value.
#[derive(Clone, Debug, PartialEq)]
pub enum Draw {
    /// An integer in `min..=max`, shrinking towards `shrink_towards`.
    Integer {
        /// The smallest value the draw allows.
        min: Integer,
        /// The largest value the draw allows.
        max: Integer,
        /// The value the shrinker moves towards, within `min..=max`.
        shrink_towards: Integer,
        /// The value drawn.
        value: Integer,
    },
    /// A boolean that is true with probability `p`.
    Boolean {
        /// The probability of `true`.
        p: f64,
        /// The value drawn.
        value: bool,
    },
    /// A float in `min..=max`.
    Float {
        /// The smallest value the draw allows.
        min: f64,
        /// The largest value the draw allows.
        max: f64,
        /// Whether the draw allows NaN.
        allow_nan: bool,
        /// Whether the draw allows the infinities.
        allow_infinity: bool,
        /// The smallest nonzero magnitude the draw allows.
        smallest_nonzero_magnitude: f64,
        /// The value drawn.
        value: f64,
    },
    /// A byte string of `min_size..=max_size` bytes.
    Bytes {
        /// The fewest bytes the draw allows.
        min_size: usize,
        /// The most bytes the draw allows.
        max_size: usize,
        /// The value drawn.
        value: Vec<u8>,
    },
    /// A string of `min_size..=max_size` codepoints from `alphabet`.
    String {
        /// The fewest codepoints the draw allows.
        min_size: usize,
        /// The most codepoints the draw allows.
        max_size: usize,
        /// The allowed codepoints, as inclusive ranges.
        alphabet: Vec<(u32, u32)>,
        /// Whether a regex or grammar constrains the string beyond its
        /// alphabet and sizes.
        constrained: bool,
        /// The codepoints drawn.
        value: Vec<u32>,
    },
    /// A cloned test case: the draws and spans it made.
    Clone {
        /// The clone's draws.
        nodes: Vec<Node>,
        /// The clone's spans.
        spans: Vec<Span>,
    },
}

/// A labelled run of consecutive draws, such as one generator's draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    /// The index of the span's first draw.
    pub start: usize,
    /// The index one past the span's last draw.
    pub end: usize,
    /// The span's label, identifying the generator that formed it.
    pub label: u64,
    /// How many spans enclose this one.
    pub depth: u32,
    /// The index of the enclosing span, if any.
    pub parent: Option<usize>,
    /// Whether the test discarded what the span drew, as a rejected filter
    /// attempt does.
    pub discarded: bool,
}

const REPORT_VERSION: u8 = 1;

/// Encode `choices` as the engine's choice-sequence encoding, which
/// `hegel_run_propose` takes as a prefix.
pub(crate) fn encode_choices(choices: &[Choice]) -> Vec<u8> {
    let mut out = Vec::new();
    put_choices(&mut out, choices);
    out
}

fn put_choices(out: &mut Vec<u8>, choices: &[Choice]) {
    out.extend_from_slice(&(choices.len() as u32).to_le_bytes());
    for choice in choices {
        match choice {
            Choice::Integer(value) => {
                out.extend_from_slice(&[0, 10]);
                out.extend_from_slice(&17u32.to_le_bytes());
                out.extend_from_slice(&value.to_le_bytes());
            }
            Choice::Boolean(value) => out.extend_from_slice(&[1, u8::from(*value)]),
            Choice::Float(value) => {
                out.push(2);
                out.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            Choice::Bytes(value) => {
                out.push(3);
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                out.extend_from_slice(value);
            }
            Choice::String(value) => {
                out.push(4);
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                for codepoint in value {
                    out.extend_from_slice(&codepoint.to_le_bytes());
                }
            }
            Choice::Clone(children) => {
                out.push(5);
                put_choices(out, children);
            }
        }
    }
}

/// Decode a case report from `hegel_run_case_report`.
pub(crate) fn decode_report(bytes: &[u8]) -> Report {
    parse_report(bytes)
        .unwrap_or_else(|| hegel_internal_error!("libhegel returned a malformed case report"))
}

fn parse_report(bytes: &[u8]) -> Option<Report> {
    let mut r = Reader { bytes, pos: 0 };
    if r.u8()? != REPORT_VERSION {
        return None;
    }
    let id = r.u64()?;
    let status = match r.u8()? {
        0 => Status::Valid,
        1 => Status::Invalid,
        2 => Status::Overrun,
        3 => Status::Interesting,
        _ => return None,
    };
    let origin = r.opt(Reader::str)?;
    let (nodes, spans) = r.stream()?;
    let targets = r.list(|r| Some((r.str()?, r.f64()?)))?;
    let events = r.list(|r| Some((r.str()?, r.opt(Reader::f64)?)))?;
    (r.pos == bytes.len()).then_some(Report {
        id,
        status,
        origin,
        nodes,
        spans,
        targets,
        events,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.bytes.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(bytes)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }

    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.array()?))
    }

    fn size(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(self.u64()?))
    }

    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    fn int(&mut self) -> Option<Integer> {
        let len = self.u32()? as usize;
        Integer::from_le_bytes(self.take(len)?)
    }

    fn opt<T>(&mut self, read: impl FnOnce(&mut Self) -> Option<T>) -> Option<Option<T>> {
        if self.bool()? {
            read(self).map(Some)
        } else {
            Some(None)
        }
    }

    fn list<T>(&mut self, mut read: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let len = self.u32()?;
        (0..len).map(|_| read(self)).collect()
    }

    fn stream(&mut self) -> Option<(Vec<Node>, Vec<Span>)> {
        let nodes = self.list(Reader::node)?;
        let spans = self.list(|r| {
            Some(Span {
                start: r.size()?,
                end: r.size()?,
                label: r.u64()?,
                depth: r.u32()?,
                parent: r.opt(Reader::size)?,
                discarded: r.bool()?,
            })
        })?;
        Some((nodes, spans))
    }

    fn node(&mut self) -> Option<Node> {
        let tag = self.u8()?;
        let forced = self.bool()?;
        let draw = match tag {
            0 => Draw::Integer {
                min: self.int()?,
                max: self.int()?,
                shrink_towards: self.int()?,
                value: self.int()?,
            },
            1 => Draw::Boolean {
                p: self.f64()?,
                value: self.bool()?,
            },
            2 => Draw::Float {
                min: self.f64()?,
                max: self.f64()?,
                allow_nan: self.bool()?,
                allow_infinity: self.bool()?,
                smallest_nonzero_magnitude: self.f64()?,
                value: self.f64()?,
            },
            3 => Draw::Bytes {
                min_size: self.size()?,
                max_size: self.size()?,
                value: {
                    let len = self.u32()? as usize;
                    self.take(len)?.to_vec()
                },
            },
            4 => Draw::String {
                min_size: self.size()?,
                max_size: self.size()?,
                constrained: self.bool()?,
                alphabet: self.list(|r| Some((r.u32()?, r.u32()?)))?,
                value: self.list(Reader::u32)?,
            },
            5 => {
                let (nodes, spans) = self.stream()?;
                Draw::Clone { nodes, spans }
            }
            _ => return None,
        };
        Some(Node { forced, draw })
    }
}

#[cfg(test)]
#[path = "../tests/embedded/explore_tests.rs"]
mod tests;
