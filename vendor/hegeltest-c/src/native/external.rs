//! External generation: a run whose generation phase takes each case from
//! its driver as data instead of drawing it itself.
//!
//! The driver answers each request with a [`Next`]: a [`Proposal`] — a
//! choice prefix replayed positionally, punning a value that does not fit
//! its draw, followed by a [`Tail`] for every draw past it — or the end of
//! generation. A case built from a proposal is a function of the proposal
//! alone: a random tail is seeded by the proposal, never by the engine's
//! own generator. After running it, the engine posts the case's report
//! ([`encode_report`]) for the driver to read: every realized choice with
//! its kind and bounds, the spans, the status and origin, and the targets
//! and events the test recorded.

use alloc::vec::Vec;

use crate::control::InternalError;
use crate::native::bignum::BigInt;
use crate::native::core::choices::ChoiceTemplate;
use crate::native::core::{ChoiceData, ChoiceNode, ChoiceValue, NativeTestCase, Span, Status};
use crate::native::rng::EngineRng;
use crate::native::test_runner::RunResult;

/// The driver's answer to a proposal request.
pub(crate) enum Next {
    Run(Proposal),
    /// End generation; the run continues to shrinking and its report.
    Done,
}

/// One generation case, as data.
pub(crate) struct Proposal {
    /// Echoed in the case's report.
    pub id: u64,
    pub prefix: Vec<ChoiceValue>,
    pub tail: Tail,
}

/// How a proposal's case draws past its prefix.
#[derive(Clone, Copy)]
pub(crate) enum Tail {
    /// At random, from a generator seeded with `seed`.
    Random { seed: u64 },
    /// Every draw at its simplest value.
    Simplest,
}

impl Proposal {
    /// The test case this proposal describes, drawing at most `max_size`
    /// choices (never fewer than the prefix).
    pub(crate) fn test_case(&self, max_size: usize) -> Result<NativeTestCase, InternalError> {
        match self.tail {
            Tail::Random { seed } => {
                NativeTestCase::for_probe(&self.prefix, EngineRng::seeded(seed), max_size)
            }
            Tail::Simplest => Ok(NativeTestCase::for_choices_and_template(
                &self.prefix,
                None,
                Some(ChoiceTemplate::simplest(None)?),
                max_size,
                None,
            )),
        }
    }
}

/// Version byte leading every report.
pub(crate) const REPORT_VERSION: u8 = 1;

/// Encode the report of the case built from proposal `id`.
///
/// All integers are little-endian. An `int` is a u32 byte count followed by
/// the value's two's-complement bytes; a `str` is a u32 byte count followed
/// by UTF-8; `size` is a u64.
///
/// ```text
/// report  = version:u8 id:u64 status:u8 origin:opt<str> stream
///           targets:u32 (label:str score:f64)*  -- sorted by label
///           events:u32 (label:str value:opt<f64>)*  -- in recording order
/// status  = 0 valid | 1 invalid | 2 overrun | 3 interesting
/// opt<T>  = 0 | 1 T
/// stream  = nodes:u32 node* spans:u32 span*
/// node    = tag:u8 forced:u8 body
///   tag 0 integer: min:int max:int shrink_towards:int value:int
///   tag 1 boolean: p:f64 value:u8
///   tag 2 float:   min:f64 max:f64 allow_nan:u8 allow_infinity:u8
///                  smallest_nonzero_magnitude:f64 value:f64
///   tag 3 bytes:   min_size:size max_size:size len:u32 byte*
///   tag 4 string:  min_size:size max_size:size constrained:u8
///                  intervals:u32 (start:u32 end:u32)* len:u32 codepoint:u32*
///   tag 5 clone:   stream
/// span    = start:u64 end:u64 label:u64 depth:u32 parent:opt<u64>
///           discarded:u8
/// ```
///
/// Floats are their IEEE-754 bit patterns. A string's `constrained` is 1
/// when a regex or grammar constrains it beyond its alphabet; its
/// intervals are the alphabet's inclusive codepoint ranges.
pub(crate) fn encode_report(id: u64, run: &RunResult) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(REPORT_VERSION);
    put_u64(&mut out, id);
    out.push(match run.status {
        Status::Valid => 0,
        Status::Invalid => 1,
        Status::EarlyStop => 2,
        Status::Interesting => 3,
    });
    put_opt(&mut out, run.origin.as_deref(), |out, origin| {
        put_str(out, origin)
    });
    put_stream(&mut out, &run.nodes, &run.spans);
    let mut targets: Vec<(&str, f64)> = run
        .target_observations
        .iter()
        .map(|(label, score)| (label.as_str(), *score))
        .collect();
    targets.sort_by(|a, b| a.0.cmp(b.0));
    put_len(&mut out, targets.len());
    for (label, score) in targets {
        put_str(&mut out, label);
        put_f64(&mut out, score);
    }
    put_len(&mut out, run.events.len());
    for (label, value) in &run.events {
        put_str(&mut out, label);
        put_opt(&mut out, *value, put_f64);
    }
    out
}

fn put_stream(out: &mut Vec<u8>, nodes: &[ChoiceNode], spans: &[Span]) {
    put_len(out, nodes.len());
    for node in nodes {
        put_node(out, node);
    }
    put_len(out, spans.len());
    for span in spans {
        put_u64(out, span.start as u64);
        put_u64(out, span.end as u64);
        put_u64(out, span.label);
        out.extend_from_slice(&span.depth.to_le_bytes());
        put_opt(out, span.parent, |out, parent| put_u64(out, parent as u64));
        out.push(u8::from(span.discarded));
    }
}

fn put_node(out: &mut Vec<u8>, node: &ChoiceNode) {
    let forced = u8::from(node.was_forced);
    match &node.data {
        ChoiceData::Integer(kind, value) => {
            out.extend_from_slice(&[0, forced]);
            put_int(out, &kind.min_value());
            put_int(out, &kind.max_value());
            put_int(out, &kind.clamped_shrink_towards());
            put_int(out, value);
        }
        ChoiceData::Boolean(kind, value) => {
            out.extend_from_slice(&[1, forced]);
            put_f64(out, kind.p);
            out.push(u8::from(*value));
        }
        ChoiceData::Float(kind, value) => {
            out.extend_from_slice(&[2, forced]);
            put_f64(out, kind.min_value);
            put_f64(out, kind.max_value);
            out.push(u8::from(kind.allow_nan));
            out.push(u8::from(kind.allow_infinity));
            put_f64(out, kind.smallest_nonzero_magnitude);
            put_f64(out, *value);
        }
        ChoiceData::Bytes(kind, value) => {
            out.extend_from_slice(&[3, forced]);
            put_u64(out, kind.min_size as u64);
            put_u64(out, kind.max_size as u64);
            put_len(out, value.len());
            out.extend_from_slice(value);
        }
        ChoiceData::String(kind, value) => {
            out.extend_from_slice(&[4, forced]);
            put_u64(out, kind.min_size as u64);
            put_u64(out, kind.max_size as u64);
            out.push(u8::from(kind.constraint.is_some()));
            put_len(out, kind.intervals.intervals.len());
            for &(start, end) in &kind.intervals.intervals {
                out.extend_from_slice(&start.to_le_bytes());
                out.extend_from_slice(&end.to_le_bytes());
            }
            put_len(out, value.len());
            for codepoint in value {
                out.extend_from_slice(&codepoint.to_le_bytes());
            }
        }
        ChoiceData::Clone(stream) => {
            out.extend_from_slice(&[5, forced]);
            put_stream(out, stream.nodes(), stream.spans());
        }
    }
}

fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u32).to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_f64(out: &mut Vec<u8>, value: f64) {
    put_u64(out, value.to_bits());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_len(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn put_int(out: &mut Vec<u8>, value: &BigInt) {
    let bytes = value.to_signed_bytes_le();
    put_len(out, bytes.len());
    out.extend_from_slice(&bytes);
}

fn put_opt<T>(out: &mut Vec<u8>, value: Option<T>, put: impl FnOnce(&mut Vec<u8>, T)) {
    match value {
        Some(value) => {
            out.push(1);
            put(out, value);
        }
        None => out.push(0),
    }
}
