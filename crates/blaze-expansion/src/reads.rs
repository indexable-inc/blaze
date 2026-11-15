//! What a run asked of the outside, and what it was told: an [`Ask`] and the [`Digest`] of its
//! answer ([`NOTHING`](crate::NOTHING) for absence). A run's asks, sorted and each once, are its
//! read list ([`ReadList`]); the asks alone are its shape, filed under the record's key, and the
//! answers with the key make the address its value is filed under (`blaze_keyed::outside`).
//!
//! Paths are bytes the driver writes against the run's roots (`$pkg/src/lib.rs`,
//! `$out/bindings.rs`; blaze-usercode's `roots`), so a record is the same in another checkout.
//! A span question names its span against the call's numbering ([`crate::tokens::Numbering`]),
//! never by an absolute position: the absolute answer (a line, a byte range) is the read's
//! digest, so a shift above the call misses that one record and nothing else (rule 71).

use std::{alloc::Global, fmt};

use blaze_keyed::record::{Fact, FactKind, StableId, fact_id};

use crate::{
    Digest,
    flat::{Corrupt, Why},
    tokens::RelativeRange,
};

/// What a span question asks of a span of the call (proc_macro's `Span` methods).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SpanQuestion {
    /// `Span::file`, `Span::local_file`: the path of its file.
    File,
    /// `Span::line`, `Span::column`, `Span::start`, `Span::end`: where it is by line and column.
    LineColumn,
    /// `Span::byte_range`: its absolute byte range.
    ByteRange,
    /// `Span::source_text`: the text it covers, comments and spaces included.
    SourceText,
    /// `Span::parent`: the span of the call it came from.
    Parent,
    /// `Span::source`: the span it was written at.
    Source,
}

impl SpanQuestion {
    fn byte(self) -> u8 {
        match self {
            SpanQuestion::File => 0,
            SpanQuestion::LineColumn => 1,
            SpanQuestion::ByteRange => 2,
            SpanQuestion::SourceText => 3,
            SpanQuestion::Parent => 4,
            SpanQuestion::Source => 5,
        }
    }

    fn of(byte: u8) -> Option<SpanQuestion> {
        Some(match byte {
            0 => SpanQuestion::File,
            1 => SpanQuestion::LineColumn,
            2 => SpanQuestion::ByteRange,
            3 => SpanQuestion::SourceText,
            4 => SpanQuestion::Parent,
            5 => SpanQuestion::Source,
            _ => return None,
        })
    }
}

/// A span of the call by its numbering: an anchor ordinal and a range from that anchor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SpanRef {
    pub anchor: u32,
    pub range: RelativeRange,
}

/// One thing a run asked of the outside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask<'a> {
    /// A path opened to read or run: the digest of its bytes, a fixed digest for a directory,
    /// [`NOTHING`](crate::NOTHING) for no file.
    File(&'a [u8]),
    /// A path only looked at (`stat`, `access`, `readlink`): what it is (kind, size, a link's
    /// target), not what is in it.
    Look(&'a [u8]),
    /// A directory listed: the digest of its sorted `(name, kind)` entries.
    List(&'a [u8]),
    /// Everything below a directory (a `rerun-if-changed` of a directory, which cargo checks
    /// whole).
    Tree(&'a [u8]),
    /// An environment variable by name: the digest of its value, or nothing.
    Env(&'a [u8]),
    /// The whole environment walked: one ask of the walk set (`blaze_usercode::confine`), its
    /// answer the digest of every `(name, value)` of the set there is.
    EnvWalk,
    /// A question about a span of the call.
    Span(SpanQuestion, SpanRef),
    /// `Span::join` of two spans of the call: their join, or none.
    Join(SpanRef, SpanRef),
    /// The edition of the call's context of this ordinal (printing a keyword of some editions).
    Edition(u32),
}

impl Ask<'_> {
    fn tag(&self) -> u8 {
        match self {
            Ask::File(_) => 0,
            Ask::Look(_) => 1,
            Ask::List(_) => 2,
            Ask::Tree(_) => 3,
            Ask::Env(_) => 4,
            Ask::EnvWalk => 5,
            Ask::Span(..) => 6,
            Ask::Join(..) => 7,
            Ask::Edition(_) => 8,
        }
    }

    /// Appends the ask's bytes: its tag, then its fields, so that the bytes of two asks compare
    /// as the asks do and one ask's bytes never begin another's.
    pub fn encode(&self, out: &mut Vec<u8, Global>) {
        out.push(self.tag());
        let span = |out: &mut Vec<u8, Global>, it: &SpanRef| {
            out.extend_from_slice(&it.anchor.to_be_bytes());
            out.extend_from_slice(&it.range.start.to_be_bytes());
            out.extend_from_slice(&it.range.end.to_be_bytes());
        };
        match self {
            Ask::File(path) | Ask::Look(path) | Ask::List(path) | Ask::Tree(path) | Ask::Env(path) => {
                let len = u32::try_from(path.len()).expect("a path is under 4 GiB");
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(path);
            }
            Ask::EnvWalk => {}
            Ask::Span(question, at) => {
                out.push(question.byte());
                span(out, at);
            }
            Ask::Join(a, b) => {
                span(out, a);
                span(out, b);
            }
            Ask::Edition(ctx) => out.extend_from_slice(&ctx.to_be_bytes()),
        }
    }

    /// The ask at the start of `bytes`, and how many bytes it is.
    pub fn decode(bytes: &[u8]) -> Result<(Ask<'_>, usize), Corrupt> {
        let corrupt = |at, why| Corrupt { kind: "read list", at, why };
        let word = |at: usize| -> Result<u32, Corrupt> {
            bytes
                .get(at..at + 4)
                .map(|it| u32::from_be_bytes(it.try_into().expect("four bytes")))
                .ok_or(corrupt(at, Why::Short))
        };
        let span = |at: usize| -> Result<SpanRef, Corrupt> {
            Ok(SpanRef {
                anchor: word(at)?,
                range: RelativeRange { start: word(at + 4)?, end: word(at + 8)? },
            })
        };
        let Some(&tag) = bytes.first() else { return Err(corrupt(0, Why::Short)) };
        Ok(match tag {
            0..=4 => {
                let len = word(1)? as usize;
                let path = bytes.get(5..5 + len).ok_or(corrupt(5, Why::Short))?;
                let ask = match tag {
                    0 => Ask::File(path),
                    1 => Ask::Look(path),
                    2 => Ask::List(path),
                    3 => Ask::Tree(path),
                    _ => Ask::Env(path),
                };
                (ask, 5 + len)
            }
            5 => (Ask::EnvWalk, 1),
            6 => {
                let byte = *bytes.get(1).ok_or(corrupt(1, Why::Short))?;
                let question = SpanQuestion::of(byte).ok_or(corrupt(1, Why::Tag(byte)))?;
                (Ask::Span(question, span(2)?), 14)
            }
            7 => (Ask::Join(span(1)?, span(13)?), 25),
            8 => (Ask::Edition(word(1)?), 5),
            other => return Err(corrupt(0, Why::Tag(other))),
        })
    }
}

impl fmt::Display for Ask<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = String::from_utf8_lossy;
        match self {
            Ask::File(it) => write!(f, "the file `{}`", path(it)),
            Ask::Look(it) => write!(f, "what `{}` is", path(it)),
            Ask::List(it) => write!(f, "the listing of `{}`", path(it)),
            Ask::Tree(it) => write!(f, "everything below `{}`", path(it)),
            Ask::Env(it) => write!(f, "the variable `{}`", path(it)),
            Ask::EnvWalk => f.write_str("the whole environment (its walk set)"),
            Ask::Span(question, at) => write!(f, "{question:?} of the call's span {at:?}"),
            Ask::Join(a, b) => write!(f, "the join of the call's spans {a:?} and {b:?}"),
            Ask::Edition(ctx) => write!(f, "the edition of the call's context {ctx}"),
        }
    }
}

/// An ask held by its bytes, for an error that outlives the run (rule 19).
#[derive(Clone, PartialEq, Eq)]
pub struct AskBuf(Vec<u8, Global>);

impl AskBuf {
    pub fn of(ask: &Ask<'_>) -> AskBuf {
        let mut bytes = Vec::new_in(Global);
        ask.encode(&mut bytes);
        AskBuf(bytes)
    }

    pub fn ask(&self) -> Ask<'_> {
        Ask::decode(&self.0).expect("an AskBuf holds an ask it encoded").0
    }
}

impl fmt::Display for AskBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.ask(), f)
    }
}

impl fmt::Debug for AskBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AskBuf({})", self.ask())
    }
}

/// The current answer of each ask, from where a result is to be used: the view's tree after
/// the request's barrier for paths, the request's environment for variables, the database for
/// span questions. A driver's, taken as a type parameter.
pub trait Answers {
    /// The digest of `ask`'s answer now, [`NOTHING`](crate::NOTHING) when nothing is there; an
    /// answer that can't be had (a file there that can't be read) is an error, never nothing.
    fn answer(&mut self, ask: &Ask<'_>) -> Result<Digest, Unanswerable>;
}

/// Why an ask has no answer now: the driver's reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unanswerable(pub &'static str);

/// Asks with the digest each answered when the run asked it, as a run makes them: in any
/// order, an ask any number of times.
pub struct Reads {
    bytes: Vec<u8, Global>,
    /// Each ask's bytes in `bytes` and its digest.
    entries: Vec<(u32, u32, Digest), Global>,
}

impl Default for Reads {
    fn default() -> Reads {
        Reads { bytes: Vec::new_in(Global), entries: Vec::new_in(Global) }
    }
}

/// An ask that answered two ways within one run: what it read moved while it ran.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Moved(pub AskBuf);

impl Reads {
    pub fn push(&mut self, ask: &Ask<'_>, answer: Digest) {
        let start = self.bytes.len() as u32;
        ask.encode(&mut self.bytes);
        self.entries.push((start, self.bytes.len() as u32, answer));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The list: sorted by ask, each ask once.
    pub fn finish(mut self) -> Result<ReadList, Moved> {
        let bytes = &self.bytes;
        let of = |entry: &(u32, u32, Digest)| &bytes[entry.0 as usize..entry.1 as usize];
        self.entries.sort_by(|a, b| of(a).cmp(of(b)).then(a.2.cmp(&b.2)));
        let mut shape = Vec::new_in(Global);
        let mut answers = Vec::with_capacity_in(self.entries.len(), Global);
        let mut last: Option<&(u32, u32, Digest)> = None;
        for entry in &self.entries {
            if let Some(prev) = last
                && of(prev) == of(entry)
            {
                if prev.2 != entry.2 {
                    let ask = Ask::decode(of(entry)).expect("encoded here").0;
                    return Err(Moved(AskBuf::of(&ask)));
                }
                continue;
            }
            shape.extend_from_slice(of(entry));
            answers.push(entry.2);
            last = Some(entry);
        }
        Ok(ReadList { shape, answers })
    }
}

/// A run's read list: its asks sorted and each once ([`ReadList::shape`]), and the digest
/// each answered, in the same order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReadList {
    shape: Vec<u8, Global>,
    answers: Vec<Digest, Global>,
}

impl ReadList {
    /// The list of a run that read nothing: its key alone is its validity.
    pub fn empty() -> ReadList {
        ReadList { shape: Vec::new_in(Global), answers: Vec::new_in(Global) }
    }

    /// The asks' bytes: what is filed under the key.
    pub fn shape(&self) -> &[u8] {
        &self.shape
    }

    pub fn answers(&self) -> &[Digest] {
        &self.answers
    }

    pub fn asks(&self) -> Asks<'_> {
        Asks { bytes: &self.shape }
    }

    pub fn is_empty(&self) -> bool {
        self.answers.is_empty()
    }

    /// The asks with their answers.
    pub fn iter(&self) -> impl Iterator<Item = (Ask<'_>, Digest)> + '_ {
        self.asks().map(|it| it.expect("a ReadList holds asks it encoded")).zip(self.answers.iter().copied())
    }

    /// The list in the core's vocabulary: each ask's fact id ([`ask_fact`]) with
    /// its answer, in the list's order.
    pub fn facts(&self) -> Vec<Fact, Global> {
        let mut facts = Vec::with_capacity_in(self.answers.len(), Global);
        let mut bytes = Vec::new_in(Global);
        for (ask, digest) in self.iter() {
            bytes.clear();
            ask.encode(&mut bytes);
            facts.push(Fact { id: ask_fact(&bytes), digest });
        }
        facts
    }
}

/// The asks of a shape's bytes, read in place.
#[derive(Clone)]
pub struct Asks<'a> {
    bytes: &'a [u8],
}

impl<'a> Asks<'a> {
    pub fn of(shape: &'a [u8]) -> Asks<'a> {
        Asks { bytes: shape }
    }
}

impl<'a> Iterator for Asks<'a> {
    type Item = Result<Ask<'a>, Corrupt>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        match Ask::decode(self.bytes) {
            Ok((ask, len)) => {
                self.bytes = &self.bytes[len..];
                Some(Ok(ask))
            }
            Err(corrupt) => {
                self.bytes = &[];
                Some(Err(corrupt))
            }
        }
    }
}

/// The fact id of an ask's bytes ([`Ask::encode`]): an outside read, in the core's vocabulary
/// (what explain shows of a value's read list).
pub fn ask_fact(ask: &[u8]) -> Digest {
    fact_id(FactKind::Late, &StableId::of(ask, 0), b"")
}
